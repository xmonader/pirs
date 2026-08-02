# pirs project site

Static site for https://xmonader.github.io/pirs/

| Page | Purpose |
|------|---------|
| `index.html` | What it is + install |
| `extend.html` | Customize with Rhai scripts |
| `strategies.html` | How multi-step / dual-model runs work |
| `features.html` | Short feature list |
| `examples.html` | Copy-paste recipes |

```bash
python3 -m http.server 8080 --directory site
```

Deploy: push to `main` → `.github/workflows/pages.yml`.
Pages source must be **GitHub Actions**.
