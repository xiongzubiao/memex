// Shared helpers for the publish scripts.

/// For prerelease versions, npm requires --tag to avoid auto-promotion to `latest`.
/// "0.1.0-rc1" → "rc"; "0.1.0-bootstrap.0" → "bootstrap"; stable (no `-`) → null.
export function distTagFor(version) {
  const m = version.match(/-([a-z][a-z0-9-]*)/i);
  return m ? m[1] : null;
}
