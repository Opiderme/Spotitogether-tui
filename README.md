# Spotitogether

Écoute Spotify synchronisée entre amis, dans le terminal. Un hôte lance un salon
sur sa machine, les autres le rejoignent avec un code, et chacun entend la même
musique sur son propre compte Spotify.

> État : phase 5 (salon, synchro, TUI, permissions, reconnexion, autoplay,
> MPRIS, accès Internet et paquets).

## Prérequis

- Un compte **Spotify Premium** (obligatoire pour piloter la lecture via l'API).
- Un lecteur Spotify ouvert : l'app officielle, `spotifyd`, ou ton téléphone.
- Rust 1.88+.

## Installation

Sur Arch Linux, avec paru : ajoute ce dépôt de PKGBUILD à la fin de
`~/.config/paru/paru.conf` (ou `/etc/paru.conf`)…

```ini
[spotitogether]
Url = https://github.com/Opiderme/Spotitogether-tui
Path = deploy/aur/spotitogether-git
```

…puis :

```sh
paru -Sy --pkgbuilds             # télécharge le dépôt (accepter les changements)
paru -S spotitogether-git        # compile et installe
paru -Syu --devel                # plus tard : mises à jour, dont les nouveaux commits
```

Sans paru : `git clone` du dépôt, puis `makepkg -si` dans
`deploy/aur/spotitogether-git`.

```sh
# Ailleurs
cargo install --locked --git https://github.com/Opiderme/Spotitogether-tui spotitogether
```

Le serveur seul existe aussi en image Docker (voir le `Dockerfile`) et en
service utilisateur systemd (`systemctl --user enable --now spotitogether`).
Les PKGBUILD sont dans `deploy/aur/`.

## Premiers pas

```sh
spotitogether
```

L'écran d'accueil propose de **créer un salon** ou d'en **rejoindre un** en
collant le lien d'invitation. La première fois, il propose aussi de configurer
Spotify (création de ton app, voir plus bas) puis de te connecter.

- **Créer un salon** : le salon tourne dans le programme. Le lien d'invitation
  s'affiche en bas de l'écran et `i` le copie dans le presse-papiers. Il marche
  depuis n'importe où, sans port à ouvrir ni logiciel à installer (connexion
  pair-à-pair, voir [`deploy/README.md`](deploy/README.md)). Quitter (`q` deux
  fois) ferme le salon.
- **Rejoindre** : colle le lien (`spotitogether://<clé>/K7XQ2M`). Le dernier lien
  est mémorisé. `spotitogether <lien>` rejoint directement, et
  l'entrée de bureau ouvre les liens `spotitogether://`.

Pour un salon qui reste ouvert sans toi (serveur, Docker, systemd), il y a
`spotitogether serve` ; `spotitogether join <lien>` rejoint sans passer par
l'accueil. Les commandes `setup`, `login` et `spotify …` restent disponibles.

Dans un salon, l'interface plein écran se pilote au clavier et à la souris :

| Touche | Action |
| --- | --- |
| `/` puis Entrée | chercher un morceau ; Entrée (ou clic) l'ajoute à la file |
| `l` | ta bibliothèque (titres likés, playlists) : filtrer (`/`, par titre, artiste ou album), ajouter un titre (Entrée), toute la liste ou les titres filtrés (`t`), les jouer tout de suite dans l'ordre (`p`) ou en aléatoire (`s`) |
| espace, `s`, ← → | lecture/pause du salon, morceau suivant, ±10 s |
| `j` `k`, `J` `K`, `t`, `d` | sélection, déplacement, passage en tête de file et retrait |
| `v` | visualiseur plein écran : la pochette et une animation qui réagit au son, 10 styles (`←` `→` ou `1`–`0` ; `c` pochette en blocs, ASCII couleur, ASCII pur ou vraie image ; `Échap` retour) |
| `y` | paroles synchronisées plein écran, 8 styles d'apparition (`←` `→` ou `1`–`8` : dispersé, karaoké, chute, décodage, vague, explosion, néon, bulles ; `v` bascule vers le visualiseur) |
| `+` `-`, `m` | ton volume, muet pour le titre en cours (toi seul ; le son revient au suivant) |
| `f` | j'aime / je n'aime plus (titre en cours, ou sélectionné dans la recherche et la bibliothèque) |
| `r` | resynchroniser ton Spotify |
| `c` | pochette en ASCII, en blocs colorés ou en vraie image (Sixel/Kitty) |
| `p` | permissions des membres (hôte) |
| `A` | playlist d'autoplay, jouée quand la file est vide (hôte) |
| `i` | copier le lien d'invitation |
| `?`, `q` | aide, quitter |

À la souris : boutons, barre de progression (clic pour se déplacer), glisser une
ligne de la file pour la déplacer, molette pour défiler. Les couleurs suivent le
thème Caelestia s'il est présent (rechargé quand le fond d'écran change).
`join --plain` garde l'ancien client en lignes de commande.

Le visualiseur écoute la sortie son de ton PC (la « source moniteur » de
PipeWire/PulseAudio, comme `cava`) pour réagir à la musique ; rien n'est
enregistré ni envoyé, et la capture s'arrête quand tu le fermes. Sans serveur son,
l'animation est simulée.

Les paroles viennent de [LRCLIB](https://lrclib.net), base libre et gratuite :
seuls le titre, l'artiste, l'album et la durée du morceau lui sont envoyés, et
les paroles sont gardées en cache dans `~/.cache/spotitogether/lyrics/`.

Le morceau du salon apparaît aussi comme lecteur MPRIS « Spotitogether » : barre
Caelestia, `playerctl` et touches multimédia (avec les droits de ton rôle). Si la
connexion coupe, le client se reconnecte seul et retrouve sa place pendant
2 minutes.

Chacun entend la musique sur son propre compte ; une dérive de plus de 1,5 s est
corrigée automatiquement.

## Rôles et permissions

| Permission | Hôte | DJ | Auditeur | Spectateur |
| --- | --- | --- | --- | --- |
| Écouter en synchro | oui | oui | oui | non |
| Ajouter à la file | oui | oui | oui, 2 en attente max | non |
| Retirer / réordonner la file, ajouter une liste entière | oui | oui | ses propres ajouts | non |
| Lecture, pause, position | oui | oui | non | non |
| Passer le morceau | oui | oui | vote (majorité) | non |

L'hôte ouvre le panneau `p` pour changer le rôle d'un membre, lui accorder ou
retirer une permission précise (exception marquée `*`), l'exclure, le bannir,
activer la salle d'attente (chaque entrée doit être admise), choisir le rôle des
nouveaux arrivants ou générer un nouveau code. Rôles, exceptions, bannis et
réglages sont gardés dans `~/.local/share/spotitogether/room-<port>.sqlite`
(`serve --ephemeral` pour ne rien enregistrer). Les droits sont vérifiés par le
serveur, quel que soit le client utilisé.

Les invitations marchent hors de ton réseau sans configuration ; pour un salon
permanent sur un serveur (Tailscale, HTTPS avec Caddy, redirection de port), voir
[`deploy/README.md`](deploy/README.md).

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
