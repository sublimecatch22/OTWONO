# Development stack

Brings up the relay, WordPress and a database so the WordPress plugin can be
tested end to end.

```
docker compose -f infrastructure/docker/docker-compose.yml up --build
```

Then:

| Service   | Address                 |
| --------- | ----------------------- |
| WordPress | http://127.0.0.1:8080   |
| Relay     | http://127.0.0.1:8788   |

The plugin directory is mounted into WordPress, so it appears under Plugins
without uploading a ZIP. Activate it, then follow `docs/WORDPRESS_SETUP.md`.

Everything binds to `127.0.0.1`, so nothing in this stack is reachable from
another machine. The passwords in the compose file are development
placeholders; do not reuse them.

To stop and remove the data:

```
docker compose -f infrastructure/docker/docker-compose.yml down -v
```
