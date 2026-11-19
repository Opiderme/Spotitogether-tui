//! Mapbox Vector Tiles (the protobuf format of OpenStreetMap vector maps),
//! decoded by hand: the format is small and only read here.
//!
//! A tile holds layers ("transportation", "water", "place"…); each feature
//! has tags and a geometry in tile units (0 – `extent`, y down).

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Text(String),
    Number(f64),
    Bool(bool),
}

impl Value {
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn number(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Point,
    Line,
    Polygon,
}

#[derive(Debug, Clone)]
pub struct Feature {
    pub kind: Kind,
    pub tags: HashMap<String, Value>,
    /// Points, lines, or polygon rings (closed), in tile units.
    pub parts: Vec<Vec<(f32, f32)>>,
}

impl Feature {
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags.get(key).and_then(Value::text)
    }
}

#[derive(Debug, Clone)]
pub struct Layer {
    pub name: String,
    pub extent: f32,
    pub features: Vec<Feature>,
}

#[derive(Debug, Clone, Default)]
pub struct VectorTile {
    pub layers: Vec<Layer>,
}

impl VectorTile {
    pub fn layer(&self, name: &str) -> Option<&Layer> {
        self.layers.iter().find(|l| l.name == name)
    }
}

/// Protobuf reader over a byte slice.
struct Pb<'a> {
    buf: &'a [u8],
    pos: usize,
}

type Result<T> = std::result::Result<T, &'static str>;

impl<'a> Pb<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.buf.get(self.pos).ok_or("varint tronqué")?;
            self.pos += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("varint trop long")
    }

    /// Field number and wire type.
    fn key(&mut self) -> Result<(u64, u64)> {
        let k = self.varint()?;
        Ok((k >> 3, k & 7))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()? as usize;
        let end = self.pos.checked_add(len).filter(|&e| e <= self.buf.len()).ok_or("longueur invalide")?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn fixed(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos + n;
        let out = self.buf.get(self.pos..end).ok_or("valeur tronquée")?;
        self.pos = end;
        Ok(out)
    }

    fn skip(&mut self, wire: u64) -> Result<()> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => {
                self.fixed(8)?;
            }
            2 => {
                self.bytes()?;
            }
            5 => {
                self.fixed(4)?;
            }
            _ => return Err("type de champ inconnu"),
        }
        Ok(())
    }

    fn packed(&mut self) -> Result<Vec<u32>> {
        let mut inner = Pb::new(self.bytes()?);
        let mut out = Vec::new();
        while !inner.done() {
            out.push(inner.varint()? as u32);
        }
        Ok(out)
    }
}

fn zigzag(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

fn value(buf: &[u8]) -> Result<Value> {
    let mut pb = Pb::new(buf);
    let mut out = Value::Bool(false);
    while !pb.done() {
        let (field, wire) = pb.key()?;
        out = match (field, wire) {
            (1, 2) => Value::Text(String::from_utf8_lossy(pb.bytes()?).into_owned()),
            (2, 5) => Value::Number(f64::from(f32::from_le_bytes(pb.fixed(4)?.try_into().unwrap()))),
            (3, 1) => Value::Number(f64::from_le_bytes(pb.fixed(8)?.try_into().unwrap())),
            (4 | 5, 0) => Value::Number(pb.varint()? as i64 as f64),
            (6, 0) => {
                let v = pb.varint()?;
                Value::Number(((v >> 1) as i64 ^ -((v & 1) as i64)) as f64)
            }
            (7, 0) => Value::Bool(pb.varint()? != 0),
            (_, w) => {
                pb.skip(w)?;
                continue;
            }
        };
    }
    Ok(out)
}

/// Draw commands to parts (MoveTo starts a part, ClosePath closes a ring).
fn geometry(commands: &[u32], kind: Kind) -> Vec<Vec<(f32, f32)>> {
    let (mut x, mut y) = (0i32, 0i32);
    let mut parts: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut i = 0;
    while i < commands.len() {
        let (cmd, count) = (commands[i] & 7, (commands[i] >> 3) as usize);
        i += 1;
        match cmd {
            1 | 2 => {
                for _ in 0..count {
                    if i + 1 >= commands.len() {
                        return parts;
                    }
                    x += zigzag(commands[i]);
                    y += zigzag(commands[i + 1]);
                    i += 2;
                    if cmd == 1 || parts.is_empty() || kind == Kind::Point {
                        parts.push(Vec::new());
                    }
                    parts.last_mut().unwrap().push((x as f32, y as f32));
                }
            }
            7 => {
                if let Some(ring) = parts.last_mut()
                    && let Some(&first) = ring.first()
                {
                    ring.push(first);
                }
            }
            _ => return parts,
        }
    }
    parts
}

fn layer(buf: &[u8]) -> Result<Layer> {
    let mut pb = Pb::new(buf);
    let (mut name, mut extent) = (String::new(), 4096.0);
    let (mut keys, mut values, mut raw) = (Vec::new(), Vec::new(), Vec::new());
    while !pb.done() {
        match pb.key()? {
            (1, 2) => name = String::from_utf8_lossy(pb.bytes()?).into_owned(),
            (2, 2) => raw.push(pb.bytes()?),
            (3, 2) => keys.push(String::from_utf8_lossy(pb.bytes()?).into_owned()),
            (4, 2) => values.push(value(pb.bytes()?)?),
            (5, 0) => extent = pb.varint()? as f32,
            (_, w) => pb.skip(w)?,
        }
    }
    let mut features = Vec::with_capacity(raw.len());
    for buf in raw {
        let mut pb = Pb::new(buf);
        let (mut kind, mut tags, mut commands) = (None, Vec::new(), Vec::new());
        while !pb.done() {
            match pb.key()? {
                (2, 2) => tags = pb.packed()?,
                (3, 0) => {
                    kind = match pb.varint()? {
                        1 => Some(Kind::Point),
                        2 => Some(Kind::Line),
                        3 => Some(Kind::Polygon),
                        _ => None,
                    }
                }
                (4, 2) => commands = pb.packed()?,
                (_, w) => pb.skip(w)?,
            }
        }
        let Some(kind) = kind else { continue };
        let tags = tags
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|kv| Some((keys.get(kv[0] as usize)?.clone(), values.get(kv[1] as usize)?.clone())))
            .collect();
        features.push(Feature { kind, tags, parts: geometry(&commands, kind) });
    }
    Ok(Layer { name, extent, features })
}

pub fn decode(buf: &[u8]) -> Result<VectorTile> {
    let mut pb = Pb::new(buf);
    let mut layers = Vec::new();
    while !pb.done() {
        match pb.key()? {
            (3, 2) => layers.push(layer(pb.bytes()?)?),
            (_, w) => pb.skip(w)?,
        }
    }
    Ok(VectorTile { layers })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }

    fn field(n: u64, wire: u64, out: &mut Vec<u8>) {
        varint(n << 3 | wire, out);
    }

    fn len(n: u64, data: &[u8], out: &mut Vec<u8>) {
        field(n, 2, out);
        varint(data.len() as u64, out);
        out.extend_from_slice(data);
    }

    fn packed(n: u64, vals: &[u32], out: &mut Vec<u8>) {
        let mut inner = Vec::new();
        for &v in vals {
            varint(u64::from(v), &mut inner);
        }
        len(n, &inner, out);
    }

    /// A tile with one road (a line) and one lake (a square ring).
    #[test]
    fn decodes_lines_polygons_and_tags() {
        let zz = |v: i32| ((v << 1) ^ (v >> 31)) as u32;
        let mut road = Vec::new();
        packed(2, &[0, 0], &mut road);
        field(3, 0, &mut road);
        varint(2, &mut road);
        packed(4, &[9, zz(10), zz(20), 18, zz(5), zz(0), zz(0), zz(7)], &mut road);
        let mut lake = Vec::new();
        packed(2, &[1, 1], &mut lake);
        field(3, 0, &mut lake);
        varint(3, &mut lake);
        packed(4, &[9, zz(0), zz(0), 26, zz(10), zz(0), zz(0), zz(10), zz(-10), zz(0), 15], &mut lake);
        let mut text = Vec::new();
        len(1, b"primary", &mut text);
        let mut water = Vec::new();
        len(1, b"lake", &mut water);
        let mut layer = Vec::new();
        len(1, b"mixed", &mut layer);
        len(2, &road, &mut layer);
        len(2, &lake, &mut layer);
        len(3, b"class", &mut layer);
        len(3, b"kind", &mut layer);
        len(4, &text, &mut layer);
        len(4, &water, &mut layer);
        field(5, 0, &mut layer);
        varint(4096, &mut layer);
        let mut tile = Vec::new();
        len(3, &layer, &mut tile);

        let t = decode(&tile).unwrap();
        let l = t.layer("mixed").unwrap();
        assert_eq!(l.extent, 4096.0);
        assert_eq!(l.features[0].kind, Kind::Line);
        assert_eq!(l.features[0].tag("class"), Some("primary"));
        assert_eq!(l.features[0].parts, vec![vec![(10.0, 20.0), (15.0, 20.0), (15.0, 27.0)]]);
        assert_eq!(l.features[1].kind, Kind::Polygon);
        assert_eq!(l.features[1].tag("kind"), Some("lake"));
        assert_eq!(l.features[1].parts[0], vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]);
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        assert!(decode(&[0x1a, 0xff, 0xff, 0xff]).is_err());
        assert!(decode(&[]).unwrap().layers.is_empty());
    }
}

#[cfg(test)]
mod real_tile {
    #[test]
    #[ignore = "reads $SNAPSHOT_DIR/t.pbf"]
    fn decodes_a_real_tile() {
        let dir = std::path::PathBuf::from(std::env::var("SNAPSHOT_DIR").unwrap());
        let t = super::decode(&std::fs::read(dir.join("t.pbf")).unwrap()).unwrap();
        for l in &t.layers {
            println!("{:20} {:5} objets, extent {}", l.name, l.features.len(), l.extent);
        }
        let names: Vec<_> = t.layer("transportation_name").unwrap().features.iter().filter_map(|f| f.tag("name")).take(8).collect();
        println!("{names:?}");
        let places: Vec<_> = t.layer("place").map(|l| l.features.iter().filter_map(|f| Some((f.tag("class")?, f.tag("name")?))).collect()).unwrap_or_default();
        println!("{places:?}");
    }
}
