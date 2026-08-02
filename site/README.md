# pirs project site

Static multi-page site for [GitHub Pages](https://xmonader.github.io/pirs/).

| Page | Role |
|------|------|
| `index.html` | Home — positioning, products, bench, install |
| `programmable.html` | Rhai extensibility model + snippets |
| `strategies.html` | Strategy catalog, weak-drive phases |
| `capabilities.html` | Capability surface of the shared core |
| `examples.html` | Pasteable CLI + Rhai examples |
| `styles.css` / `script.js` | Shared theme & mobile nav |

## Local preview

```bash
# from repo root
python3 -m http.server 8080 --directory site
# open http://localhost:8080
```

## Deploy

Push to `main`. Workflow: [`.github/workflows/pages.yml`](../.github/workflows/pages.yml).

**One-time:** Settings → Pages → Source: **GitHub Actions**.

URL: **https://xmonader.github.io/pirs/**

Keep scores aligned with `qa/bench-swebench-5x5/LEADERBOARD.md`.
