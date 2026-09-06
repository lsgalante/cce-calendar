# Privacy policy — cce desktop applications

This policy covers the "cce" Google OAuth application used by the cce
desktop environment (cce-mail, cce-calendar, cce-list, and the
cce-system-interface settings app), written and used by Lucas Galante.

## What the software is

cce is a personal, open-source Linux desktop environment. It runs entirely on
the user's own computer. There is no hosted service, no server operated by the
author, and no analytics.

## Data accessed through Google

When a user signs in with Google, the software may request access to:

- Gmail (`https://mail.google.com/`) — to read and send that user's own mail
  over IMAP/SMTP in cce-mail.
- Google Tasks — to read and update that user's own task lists in cce-list.
- Google Calendar (read-only) — to display that user's own events in
  cce-calendar.
- The account's email address — to label the account in the settings app.

## How data is used and stored

Data is fetched only to display it to the same user on their own machine, and
to write back changes that user makes (a task ticked off, a mail sent). Mail,
events, and tasks are cached in plain files under the user's home directory.
OAuth tokens are stored in the user's local configuration directory. Nothing
is transmitted to any party other than Google's own APIs, and nothing is sold,
shared, or used for advertising or model training.

## Retention and deletion

All data lives on the user's computer and is deleted by removing the account
in the settings app or deleting the cache files. Access can be revoked at any
time at https://myaccount.google.com/permissions.

## Contact

mail@lucas.co
