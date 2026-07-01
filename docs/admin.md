# Admin

The `/admin` page manages the subscribed feeds and groups. Changes
take effect immediately; no restart is required.

The configuration file's `rss:` block seeds the database on the first
launch only. After any admin edit, the database is the source of
truth and the seed block is ignored on subsequent restarts.

## Add a group

Type the group name into the toolbar's **New group name** field and
submit. Group names are unique; adding an existing name returns an
error.

## Remove a group

Tap the `×` icon next to the group's heading. The confirmation prompt
lists the group name; confirming deletes the group and its feed
subscriptions. Any articles those feeds had cached stay in the cache
and continue to appear under any other group still subscribed to the
same feed.

## Add a feed

Every group has an **Add feed** form under its heading. Typing into
the input shows autocomplete suggestions from the configured feed
providers; the default provider takes a site URL and returns any RSS,
Atom, or JSON-feed links advertised on the page via `<link
rel="alternate">`.

Tap a suggestion to fill in the URL, then submit.

Only `http://` and `https://` URLs are accepted.

## Remove a feed

Tap the `×` icon next to the feed URL. The confirmation prompt lists
the URL; confirming removes only that group's subscription. If the
same feed is subscribed under multiple groups, the others are
untouched.

## Import OPML

The **Import OPML** form in the toolbar accepts an OPML 1.0 or 2.0
export from another reader. Select a file and submit.

Import rules:

- **Groups merge by name.** A category whose name matches an existing
  inkwell group adds its feeds to that group. New names create new
  groups.
- **Feeds deduplicate.** A URL already subscribed in the target group
  is skipped, not added twice. Re-importing the same file is a no-op.
- **Nested categories** use the nearest label; a feed under
  `<outline text="Tech"><outline text="Programming"><feed/></outline></outline>`
  lands in `Programming`.
- **Top-level (ungrouped) feeds** go into an auto-created group named
  `Uncategorized`.
- **Unsafe schemes are skipped.** Any `xmlUrl` that isn't `http://`
  or `https://` is counted as invalid.

Uploads are capped at 1 MiB.

After import, a flash message reports the outcome:

```
Imported 47 feed(s) into 3 new group(s); 2 duplicate(s), 1 invalid skipped.
```

| Count       | Meaning                                                          |
| ----------- | ---------------------------------------------------------------- |
| Imported    | Feeds added to the database.                                     |
| New groups  | Groups that didn't exist before this import.                     |
| Duplicates  | Feed URLs already subscribed in their target group.              |
| Invalid     | `xmlUrl` entries that failed the scheme allow-list.              |

### Exporting from other readers

| Reader        | Export path                                                     |
| ------------- | --------------------------------------------------------------- |
| NetNewsWire   | File → Export Subscriptions → OPML.                             |
| Feedly        | Settings → OPML → Export your Feedly OPML.                      |
| Inoreader     | Preferences → Folders and tags → Export → OPML.                 |
| FreshRSS      | ⚙ → Subscription management → Export.                          |
| Miniflux      | Settings → Export.                                              |
| Tiny Tiny RSS | Preferences → Feeds → OPML → Export.                            |

The resulting file imports into inkwell without modification.

## Restricting access

The admin page has no built-in authentication. When the reader is
exposed beyond a trusted network, gate `/admin` at the reverse proxy
or behind an identity provider. See
[self-hosting](self-hosting.md#admin-access-control).
