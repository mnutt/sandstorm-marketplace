Public author identity proof and public key extracted from Sandstorm package
`97291f5132208e8593a791ce6c8b63ae`, downloaded from the public app index.
No private key or application files are included.

App ID: `zxent02jw0z9ckkagz1uscdxesaf1f1wgzdunx2wrcj0jt9uf9rh`
SDK-verified fingerprint: `7116FC1150C0C96EF251B9A89324AC3921F3DE70`

The proof was emitted by Keybase as an ASCII-armored PGP MESSAGE. It reproduces
the binary-only parser's “unknown packet header version 101101” error.
It also uses a two-byte first partial-body chunk, requiring the PGP library's
legacy format compatibility feature (GnuPG accepts this encoding).

`mediawiki-public-key.pgp` and `mediawiki-proof.pgp` are the unmodified public
keyring and author proof extracted from MediaWiki SPK
`16bcca05d59606b908681c07b202752a`:
https://app-index.sandstorm.io/packages/16bcca05d59606b908681c07b202752a

App ID: `p04q48xd2kgkystkrda5n77kdns6u569wnactgh904tf7s3vev70`
SDK-verified fingerprint: `C0B2C432638DAEB0D70715C7F654B8C7D4CA3CB8`

The keyring includes a self-certified user ID plus nine third-party
certifications. Passing all of these to rPGP's self-signature verifier fails
with an issuer mismatch. The fixtures contain no private keys or application
code. Sandstorm's `spk verify` accepts the original package.
