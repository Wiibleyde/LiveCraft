# Security

## Reporting a vulnerability

Please do not open a public issue. Use **Security → Report a vulnerability** on this repository, and
include how to reproduce the problem. LiveCraft is an early alpha with a small team: we will answer as soon
as we can, and credit you if you wish.

## What to know

- **Remote control.** HTTP on port 8088 and TCP on port 8099 listen on this computer only, unless
  network access is turned on in the settings. Requests caused by foreign web pages are refused.
- **On a network,** set a password. The HTTP interface then asks for it. The TCP interface has no way
  to give one, so it closes to other computers. HTTP traffic is not encrypted: use a network you trust.
- **Secrets.** Stream keys, the remote password and SRT passphrases are sealed with your Windows account
  in the session file, and are left out of `.livecraft` project files.
- **Web inputs** run real web pages in a browser without a window. Only add pages you trust.
- **Project files** can point at any file or address on your computer. Open projects you trust.
