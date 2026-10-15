# talus-server

Self-hosted sync server for Talus **shared workspaces**. A workspace is a team's list of connection
definitions; members see each other's changes through **Tools → Shared Workspace…** in the app.

## What is shared — and what never is

Shared: engine, host, port, user, database, TLS mode, group, tunnel hosts.
Never leaves a member's Mac: passwords and tunnel secrets (Keychain only), CA / SSH-key / Snowflake-key
paths, `init_sql` (it would run on every member's session), the SSH trust-on-first-use flag and the
"remember password" preferences. SQLite files are local and cannot be shared. The server also refuses
any definition that carries a secret-looking or path-like field, whatever the client sends.

Importing a definition never overwrites the local-only settings of a connection you already have, and a
new import starts with none of them: you enter your own password (and key paths) the first time.

Trust: an editor can change where a shared connection points (host, tunnel). Add only people you trust,
and look at a pulled connection before typing your password into it for the first time.

## Run

```sh
cargo build --release -p talus-server           # → target/release/talus-server
talus-server set-user --data /var/lib/talus alice   # password from $TALUS_SERVER_PASSWORD or stdin, ≥ 12 characters
talus-server serve    --data /var/lib/talus --listen 127.0.0.1:8787
```

Accounts are created and reset by the administrator with `set-user` (there is no open sign-up).
Resetting a password ends that account's sessions. Owners add existing accounts to a workspace by
user name (viewer / editor / owner) and can remove them at any time; every member can leave.

## TLS and deployment

The server speaks plain HTTP and binds to loopback by default. Put a TLS-terminating reverse proxy in
front of it; the app refuses `http://` server addresses except `localhost`:

```caddyfile
talus.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

Everything lives in `<data>/state.json` (mode 0600, directory 0700): back that file up. Session tokens
are stored only as SHA-256 hashes, passwords as PBKDF2-HMAC-SHA256 (600 000 iterations) with a random salt.

## Behaviour worth knowing

- Every endpoint checks the caller's role on the server. A non-member gets `404` for a workspace, so its
  existence is not revealed; a member with too low a role gets `403`.
- Writes carry the revision the client last saw. A stale write is a `409` with the server's current copy,
  never a silent overwrite. Pulling takes the server's definition.
- Five failed logins for a name lock it out for 60 seconds. Sessions last 30 days.
- Limits: 16 KiB per definition, 500 connections and 200 members per workspace, 50 owned workspaces per user.
- The whole state is rewritten on each change (fine for a team; use a database if it ever grows).

## API (JSON, `Authorization: Bearer <token>`)

| Method and path | Needs |
|---|---|
| `POST /v1/login` `{user, password}` → `{token}` | — |
| `POST /v1/logout` | any session |
| `GET /v1/workspaces`, `POST /v1/workspaces` `{name}` | any session |
| `GET /v1/workspaces/{ws}` | member |
| `DELETE /v1/workspaces/{ws}` | owner |
| `PUT /v1/workspaces/{ws}/members/{user}` `{role}` | owner |
| `DELETE /v1/workspaces/{ws}/members/{user}` | owner, or the member themself |
| `GET /v1/workspaces/{ws}/connections` | viewer |
| `PUT /v1/workspaces/{ws}/connections/{uuid}` `{base_rev, data}` | editor |
| `DELETE /v1/workspaces/{ws}/connections/{uuid}?base_rev=N` | editor |
