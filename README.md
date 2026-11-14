# Spotitogether

Écoute Spotify synchronisée entre amis, dans le terminal. Un hôte lance un salon
sur sa machine, les autres le rejoignent avec un code, et chacun entend la même
musique sur son propre compte Spotify.

> État : phase 1 (connexion Spotify et commandes de test). Salons, synchro et
> interface TUI arrivent dans les phases suivantes.

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
| `protocol`, `server`, `client`, `tui` | Phases 2 à 4 |
