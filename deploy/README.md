# Héberger un salon accessible depuis Internet

Par défaut, `spotitogether serve` n'est joignable que sur ton réseau local. Pour
inviter des amis ailleurs, trois options, de la plus simple à la plus complète.
Le serveur ne transporte que l'état du salon (file, position, membres) : aucun
son ni jeton Spotify ne passe par lui.

## 1. Tailscale (recommandé entre amis)

Aucun port à ouvrir, chiffrement WireGuard de bout en bout.

1. Installe Tailscale sur la machine du salon : `sudo pacman -S tailscale`,
   `sudo systemctl enable --now tailscaled`, `sudo tailscale up`.
2. Partage la machine avec tes amis depuis la console Tailscale
   (« Share » sur la machine) ; ils installent Tailscale et acceptent l'invitation.
3. Lance le salon avec l'adresse Tailscale de la machine :

   ```sh
   spotitogether serve --public mon-pc.ton-tailnet.ts.net:7420
   ```

   L'invitation affichée (`spotitogether://mon-pc.ton-tailnet.ts.net:7420/CODE`)
   fonctionne pour tous les membres du tailnet.

## 2. Caddy en reverse proxy HTTPS

Pour un serveur avec un nom de domaine (VPS, Raspberry Pi avec redirection des
ports 80 et 443). Caddy obtient le certificat Let's Encrypt tout seul et relaie
le WebSocket sans configuration particulière.

1. Fais pointer un nom (ex. `salon.exemple.fr`) vers la machine.
2. Utilise le [`Caddyfile`](Caddyfile) fourni :

   ```sh
   sudo cp deploy/Caddyfile /etc/caddy/Caddyfile   # remplace salon.exemple.fr
   sudo systemctl reload caddy
   ```

3. Lance le salon en local uniquement, avec l'adresse publique en TLS :

   ```sh
   spotitogether serve --bind 127.0.0.1 --public salon.exemple.fr --tls
   ```

   L'invitation devient `spotitogether+tls://salon.exemple.fr/CODE` et le client
   se connecte en `wss://` sur le port 443.

## 3. Redirection de port sur ta box

La plus directe, mais le trafic n'est pas chiffré (le code du salon circule en
clair) : à réserver aux tests.

1. Redirige le port TCP 7420 de la box vers la machine du salon.
2. `spotitogether serve --public ton-ip-publique:7420`

## Sécurité

- Un mauvais code bloque l'adresse IP une minute après 5 essais.
- Active la salle d'attente (panneau `p`, touche `w`) pour admettre chaque
  personne à la main, et change le code avec `n` s'il a trop circulé.
- Le rôle d'hôte n'est donné que par le jeton affiché au lancement (ou lu
  automatiquement sur la machine du salon) : ne le partage pas.
