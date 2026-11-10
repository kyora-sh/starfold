# Security

## Reporting a vulnerability

Report vulnerabilities privately on GitHub: open the repository's **Security** tab and
choose **Report a vulnerability**
([direct link](https://github.com/kyora-sh/starfold/security/advisories/new)). Please don't
open a public issue. Say which version (`starfold --version`) and platform you found it on,
and how to reproduce it. We'll confirm we got it and keep you posted while we fix it.

## Supported versions

Fixes go into the latest release, so upgrade to get them.

## What starfold protects, and what it leaves to you

Whoever signs in to starfold gets the desktop it streams: they see the screen and, unless
starfold runs with `--read-only`, type, click, use the clipboard and move files. So:

- **Login:** on by default. Every request needs the password except the page's scripts
  and assets and `/healthz`. Without `--basic-auth-password`, `STARFOLD_PASSWORD` or
  `SELKIES_BASIC_AUTH_PASSWORD`, starfold makes up a random 128-bit one when it starts.
  A `?token=` link carries the password, so treat the link like the password. `--enable-basic-auth=false` turns the login off;
  only do that behind a proxy that signs people in itself.
- **TLS:** starfold serves plain HTTP. Away from localhost, put it behind a TLS proxy or an
  SSH tunnel, or the password and the stream cross the network unencrypted.
- **Listen address:** on Linux starfold listens on `0.0.0.0`, since a VM's display usually
  sits behind its platform's proxy. On macOS and Windows, where it streams your own
  screen, it listens on `127.0.0.1`. `--addr` changes either.
- **Other sites:** starfold only answers requests for an IP address, `localhost` or a host
  `--allowed-hosts` names, and turns away browsers' requests from other sites' pages
  unless `--allowed-origins` names them. That keeps out DNS rebinding and pages that
  open a WebSocket to starfold.
- **Files:** file transfer is off unless `--files-root` names a folder; the Files panel
  then reaches only that folder and never follows a symlink. `--file-transfers=none`
  turns transfers off.
- **Watch-only viewers:** `--read-only` refuses input, clipboard writes, the
  microphone, uploads and resizing.
