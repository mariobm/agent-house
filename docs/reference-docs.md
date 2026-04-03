# Reference Docs — How They Stay Up to Date

bhatti's API and CLI reference documentation is **generated from code**, not hand-written. This means docs can never drift from the implementation.

## Architecture

```
Source of truth          Generator              Output
─────────────────        ──────────────         ─────────────────
pkg/server/routes.go  →  gen-openapi.go      →  docs/openapi.yaml
cmd/bhatti/cli.go     →  gen-cli-docs.go     →  web/cli-docs.html
                                                 web/api-docs.html (Scalar)
```

### API Reference (Scalar + OpenAPI)

1. **`scripts/gen-openapi.go`** reads the route definitions and produces `docs/openapi.yaml` (OpenAPI 3.1).
2. **`web/api-docs.html`** is a single HTML file that loads Scalar from CDN and points at `openapi.yaml`.
3. Deploy both files to `docs.bhatti.sh` — Scalar renders the spec client-side with search, try-it-out, and code samples.

### CLI Reference

1. **`scripts/gen-cli-docs.go`** mirrors the cobra command tree and produces `web/cli-docs.html`.
2. Every flag, alias, default, and example in the HTML comes from the same cobra definitions that run when you type `bhatti create`.

## Regenerating

```bash
make docs
```

This runs both generators and overwrites the output files. Commit the result.

## CI Enforcement

`.github/workflows/docs.yml` regenerates docs on every push/PR and fails if the committed files differ from what the generators produce. This catches the "added a flag but forgot to update docs" case.

## Hosting on docs.bhatti.sh

### Option A: Caddy (recommended — already have TLS via bhatti's autocert)

Add to your Caddy config or deploy a static site:

```
docs.bhatti.sh {
    root * /srv/docs
    file_server
}
```

Files to deploy:
- `web/api-docs.html` → `/srv/docs/api/index.html`
- `docs/openapi.yaml` → `/srv/docs/api/openapi.yaml`  
- `web/cli-docs.html` → `/srv/docs/cli/index.html`

URLs:
- `https://docs.bhatti.sh/api/` — Scalar API reference
- `https://docs.bhatti.sh/cli/` — CLI reference

### Option B: GitHub Pages

Add to `.github/workflows/docs.yml`:

```yaml
  deploy:
    needs: generate
    if: github.ref == 'refs/heads/main'
    runs-on: ubuntu-latest
    permissions:
      pages: write
      id-token: write
    environment:
      name: github-pages
      url: ${{ steps.deployment.outputs.page_url }}
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-go@v5
        with:
          go-version-file: go.mod
      - run: make docs
      - name: Prepare site
        run: |
          mkdir -p _site/api _site/cli
          cp web/api-docs.html _site/api/index.html
          cp docs/openapi.yaml _site/api/openapi.yaml
          cp web/cli-docs.html _site/cli/index.html
      - uses: actions/upload-pages-artifact@v3
        with:
          path: _site
      - uses: actions/deploy-pages@v4
        id: deployment
```

Then set a CNAME record: `docs.bhatti.sh → sahil-shubham.github.io`

### Option C: Serve from the bhatti daemon itself

The bhatti server already serves `web/index.html` at `/`. You could add doc routes:

```go
// In routes():
s.mux.HandleFunc("/docs/api/", s.serveAPIDocs)
s.mux.HandleFunc("/docs/cli/", s.serveCLIDocs)
```

This keeps everything on `api.bhatti.sh/docs/` — no extra subdomain needed.

## Updating the OpenAPI spec

When you add or change an API endpoint:

1. Edit the handler in `pkg/server/routes.go`
2. Add/update the corresponding entry in `scripts/gen-openapi.go`
3. Run `make docs`
4. Commit all changed files

CI will catch it if you forget step 2-3.

## Future: Fully automated generation

The current approach requires manually keeping `gen-openapi.go` in sync with route handlers. To make it fully automatic, you could:

1. **Add Go struct tags** to request/response types and use reflection
2. **Use an OpenAPI middleware** like [huma](https://github.com/danielgtaylor/huma) that generates the spec from handler signatures
3. **Parse Go AST** to extract routes, request bodies, and responses automatically

For now, the manual-but-enforced approach works well — the CI check makes it impossible to forget.
