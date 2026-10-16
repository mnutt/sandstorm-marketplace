# Sandstorm visual assets

Retrieved 2026-10-16 from the live [Sandstorm App Market](https://apps.sandstorm.io/):

- `assets/sandstorm-logo.svg`: `/icons/raw/logo-image-purple.svg` (rendered white in the header using CSS).
- `assets/welcome-banner.png`: `/images/welcome-banner.png`.
- `assets/favicon.png`: `/favicons/favicon-32x32.png`.

These images are also distributed in the official
[sandstorm-app-market source](https://github.com/sandstorm-io/sandstorm-app-market)
under Apache 2.0; the upstream license is retained in `assets/Sandstorm-LICENSE.txt`.

The three unmodified Source Sans Pro WOFF2 fonts come from
`https://sandstorm.io/fonts/sourcesans-{light,regular,semibold}.woff2`, as referenced
by the live site's font stylesheet. Source Sans is Copyright Adobe, licensed
under the SIL Open Font License 1.1; see `assets/SourceSans-LICENSE.txt`, obtained
from https://github.com/adobe-fonts/source-sans/blob/release/LICENSE.md.

`assets/marketplace.css` is a new stylesheet for this app's templates. The original
site informs its colors, typography and visual treatment; no upstream stylesheet,
Meteor code, icon font, analytics, or CSS framework is imported. All assets and
licenses are embedded in the Rust binary and served locally in both preview and
static publication. No requests to the original site are needed at runtime.
