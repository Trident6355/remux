# oidc-image

Automatisierung im Fork: `.github/workflows/oidc-image.yml` baut zu jedem neuen Remux-Release ein Image
„Release + PRs aus `prs.txt`“ und veröffentlicht es als `ghcr.io/trident6355/remux:<version>-oidc` und `:oidc`.
Schlägt etwas fehl (Konflikt, ohne Merge geschlossener PR, Test, Start), wird nichts veröffentlicht.
Von Hand: Actions → „Remux mit OIDC bauen“ → Run workflow (optional Version, „force“).
Remux' eigene Workflows bleiben im Zweig; „Nightly“ (einziger mit Zeitplan) ist im Fork deaktiviert.
