# Spotitogether

Écoute Spotify synchronisée entre amis, dans le terminal. Un hôte lance un salon
sur sa machine, les autres le rejoignent avec un code, et chacun entend la même
musique sur son propre compte Spotify.

> État : phase 3 (salon, synchro et interface TUI plein écran). Les
> permissions par rôle arrivent en phase 4.

## Prérequis

- Un compte **Spotify Premium** (obligatoire pour piloter la lecture via l'API).
- Un lecteur Spotify ouvert : l'app officielle, `spotifyd`, ou ton téléphone.
- Rust 1.88+.

## Installation

```sh
cargo install --path crates/spotitogether
```

## Premiers pas

```sh
spotitogether setup     # crée ton app Spotify et enregistre son Client ID
spotitogether login     # autorise l'accès dans le navigateur
spotitogether spotify devices
spotitogether spotify play daft punk one more time
spotitogether spotify status
```

## Héberger et rejoindre un salon

```sh
# Terminal 1 : l'hôte lance le salon
spotitogether serve                 # affiche le code et le lien d'invitation

# Terminal 2 : l'hôte rejoint son propre salon (rôle d'hôte automatique)
spotitogether join spotitogether://127.0.0.1:7420/K7XQ2M

# Chez un ami
spotitogether join spotitogether://192.168.1.19:7420/K7XQ2M
```

`join` ouvre l'interface plein écran, pilotable au clavier et à la souris :

| Touche | Action |
| --- | --- |
| `/` puis Entrée | chercher un morceau ; Entrée (ou clic) l'ajoute à la file |
| espace, `s`, ← → | lecture/pause du salon, morceau suivant, ±10 s |
| `j` `k`, `J` `K`, `d` | sélection, déplacement et retrait dans la file |
| `r` | resynchroniser ton Spotify |
| `c` | pochette en ASCII ou en blocs colorés |
| `?`, `q` | aide, quitter |

À la souris : boutons, barre de progression (clic pour se déplacer), glisser une
ligne de la file pour la déplacer, molette pour défiler. Les couleurs suivent le
thème Caelestia s'il est présent (rechargé quand le fond d'écran change).
`join --plain` garde l'ancien client en lignes de commande.

Chacun entend la musique sur son propre compte ; une dérive de plus de 1,5 s est
corrigée automatiquement.

Chaque utilisateur crée sa propre app sur le
[dashboard Spotify](https://developer.spotify.com/dashboard) : depuis février 2026,
une app en mode développement est limitée à 5 utilisateurs. Redirect URI à
déclarer : `http://127.0.0.1:8973/callback`.

Le jeton est stocké dans le trousseau système (Secret Service : gnome-keyring,
KWallet…), ou à défaut dans `~/.local/share/spotitogether/token.json` (droits 600).

## Organisation

| Crate | Rôle |
| --- | --- |
| `spotify` | OAuth PKCE et client de l'API Web Spotify |
| `spotitogether` | Binaire et sous-commandes |
| `protocol` | Messages JSON échangés en WebSocket |
| `server` | Salon : file, minuteur de piste, diffusion de l'état |
| `client` | Connexion au salon, horloge, synchro du Spotify local |
| `tui` | Interface plein écran (ratatui), pochette ASCII, thème Caelestia |
