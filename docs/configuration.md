# Configuration

## Where things live

| | | |
|---|---|---|
| `$XDG_CONFIG_HOME/riplika/` | settings | worth backing up |
| `$XDG_DATA_HOME/riplika/` | glyph table, wordlists | rebuildable, but slowly |
| `$XDG_CACHE_HOME/riplika/rip/` | a disc, before encoding | thrown away |
| login keyring | the TMDB key | not in any file |

Three lifetimes, three places. The rip does **not** go in the system temporary
directory: on most desktops that is a tmpfs held in RAM, and a disc is eight
gigabytes - filling it takes the session down with it.

The glyph table and the wordlists are not offered as settings. They are
application data with a standard place to live, built once by `riplika build`
and then used without being thought about; there is no answer a user could give
that would beat the default. Where a rip lands *is* a real question - it wants
tens of gigabytes, and a small home partition is a good reason to put it
elsewhere - so that one is asked.


## Preferences

The window keeps settings in `$XDG_CONFIG_HOME/riplika/preferences.json`, not
GSettings - a schema has to be compiled into a system directory before the
application will start, which is a poor trade for a dozen values. A missing or
corrupt file falls back to defaults rather than refusing to launch; losing
preferences should cost a re-tick, not a launch.

The split is between policy and per-disc choice. Preferences hold what is true
of the whole library - preferred languages, whether commentary is wanted, where
the glyph table lives. The rip page holds what differs between discs: quality,
the output folder, and which of *this* disc's languages to take.

**Preferred languages** decide what starts ticked. The rip page lists the
languages the disc actually carries - taken from the scan, so there is nothing
to spell - with the preferred ones ticked and moved to the top. Order is the
order you switch them on, and the first one becomes the default track. A
language you want that is not on the disc simply does not appear; a language on
the disc that you have not asked for is still offered, just unticked.

**The MakeMKV fallback** can only be switched on if `makemkvcon` is installed.
When it is missing the row is insensitive and says so, rather than being a live
control whose promise would be broken forty minutes into a disc. The choice is
honoured by both front ends through `rip::Auto`, so the window and the command
line cannot drift apart about when MakeMKV gets involved.
