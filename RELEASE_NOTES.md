# nixception 0.6.0

This release fixes a cache-parity bug: actions built through `nix develop`
could fail to hit the cache of an otherwise byte-identical action built
through `nix build`, because of a Nix daemon quirk that had nothing to do
with the actions actually differing.

## Highlights

- **Discovered `/nix/store/…` paths are always input sources, never resolved
  to a deriver.** Preparing an action's derivation used to ask the Nix daemon
  for each discovered store path's deriver, and reference that deriver
  instead of the path itself when one was found — more precise, but it
  turned out to be unreliable rather than merely unavailable. Inside a
  `recursive-nix` sandboxed build (every real `nix build` action goes through
  one), the daemon nixception talks to is Nix's own `RestrictedStore`, which
  unconditionally strips deriver info from every reply as impure — so a real
  build's actions always resolved paths as plain sources. A caller outside
  that sandbox (`nix develop`, talking to the host daemon directly) could
  resolve some of the very same paths to derivers instead, giving an
  otherwise identical action a different derivation shape — and hash — than
  the one a real build produced, defeating the cache across that boundary.
  Every discovered path is now an input source, unconditionally: less
  precise, but deterministic from any calling context, with no daemon
  round-trip (and its context-dependent answer) involved.

See the [changelog](./CHANGELOG.md) for the full list.

## Licensing

The nixception sources are licensed under **Apache-2.0**. Binary distributions
of the server are additionally governed by **GPL-3.0** through the statically
linked `nix-compat` crate — see [NOTICE](./NOTICE) for full attribution and
licensing details.
