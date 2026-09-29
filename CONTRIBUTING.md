# Contributing to ricercar

Thanks for helping! A few ground rules keep the project healthy.

## Compatibility reports

The most valuable contribution today: tell us how ricercar behaves with your
**control point** (BubbleUPnP, Symfonium, Kazoo, mConnect…) and your **DAC**.
Open an issue with the app and version, renderer mode (UPnP AV or OpenHome),
what worked and what broke, and the diagnostic report (**Settings → About →
Copy diagnostic report**; tokens and keys are hidden). The issue template
"Compatibility report" asks for all of this. For a failure, `RUST_LOG=debug`
output helps too; the log lives in `~/.local/state/ricercar/`.
Verified reports are added to [docs/CONTROLS.md](docs/CONTROLS.md).

## Code

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- Tests must only use the `null` or `file:` sinks, never real `hw:` devices.
- Keep bit-perfect honest: anything that alters samples must clear
  `ChainInfo::bit_perfect` and show up in the signal-path view.
- UI text goes through `@tr(...)`; add the French string to
  `crates/ricercar-ui/tools/fr.json` and run `crates/ricercar-ui/tools/gen_po.py`.
- `RICERCAR_SNAPSHOT=<dir>` renders
  the main views headlessly for before/after screenshots in your PR.

## What we will not accept

ricercar implements open standards only (UPnP AV, OpenHome, MPRIS, public
APIs with user-owned keys). This repository does not host code or
instructions that use a streaming service's private or unofficial API,
extract its keys, or bypass its terms.

## License

By contributing you agree that your work is released under the MIT license.
