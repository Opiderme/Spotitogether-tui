# Spotitogether

Écoute Spotify synchronisée entre amis, dans le terminal. Un hôte lance un salon
sur sa machine, les autres le rejoignent avec un code, et chacun entend la même
musique sur son propre compte Spotify.

> État : phase 2 (salon et synchro, client en ligne de commande). L'interface
> TUI plein écran arrive en phase 3, les permissions en phase 4.

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

Dans le salon, tape un titre pour l'ajouter à la file, ou `/help` pour les
commandes (`/queue`, `/skip`, `/pause`, `/seek 1:30`…). Chacun entend la musique
sur son propre compte ; une dérive de plus de 1,5 s est corrigée automatiquement.

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
| `tui` | Phase 3 |
