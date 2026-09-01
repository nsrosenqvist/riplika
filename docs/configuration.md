# Configuration

## Where things live

| | | |
|---|---|---|
| `$XDG_CONFIG_HOME/riplika/` | settings | worth backing up |
| `$XDG_DATA_HOME/riplika/` | glyph table, wordlists, Redump datfiles | rebuildable, but slowly |
| `$XDG_CACHE_HOME/riplika/rip/` | a disc, before encoding | thrown away |
| `$XDG_CACHE_HOME/riplika/art/` | cover pictures, once fetched | thrown away |
| `$XDG_STATE_HOME/riplika/logs/` | one file per disc | kept, for reading back |
| login keyring | the TMDB key | not in any file |

Each of those has a different lifetime, which is why they are in different places. The rip does **not** go in the system temporary directory: on most desktops that is a tmpfs held in RAM, and a disc is eight gigabytes - filling it takes the session down with it.

The glyph table and the wordlists are not offered as settings. They are application data with a standard place to live, built once by `riplika build` and then used without being thought about; there is no answer a user could give that would beat the default. Where a rip lands *is* a real question - it wants tens of gigabytes, and a small home partition is a good reason to put it elsewhere - so that one is asked.


## The logs

Every disc writes its own file, all in one directory, named so that sorting them puts a season in the order it was ripped:

```
2026-08-27T2015-PARKS_AND_RECREATION_S6D1.log
2026-08-28T1930-PARKS_AND_RECREATION_S6D2.log
2026-08-29T1102-PARKS_AND_RECREATION_S6D3.log
```

A season is six or seven discs over as many evenings, and by the end there is no answering "did episode four of disc two have unrecognised glyphs?" from memory. The label is in the name so a directory listing says which disc each one was without opening it.

Written as the run happens rather than assembled at the end, and flushed line by line: the runs worth reading afterwards are the interrupted ones and the failed ones, and a log built at the end would not exist for either. Progress is left out - it arrives hundreds of times a second and says nothing the lines around it do not.

Local time rather than UTC, because these are read by someone remembering which evening they did which disc, and by one implementation shared between the window and the terminal - a season ripped partly with each would otherwise sort into an order that is neither.

## Preferences

The window keeps settings in `$XDG_CONFIG_HOME/riplika/preferences.json`, not GSettings - a schema has to be compiled into a system directory before the application will start, which is a poor trade for a dozen values. A missing or corrupt file falls back to defaults rather than refusing to launch; losing preferences should cost a re-tick, not a launch.

The split is between policy and per-disc choice. Preferences hold what is true of every disc of a kind: preferred languages, whether commentary is wanted, where each library lives. The rip page holds what differs between one disc and the next, which is the quality and which of *this* disc's languages to take.

**There are three libraries, and each has its own folder.** Video, music and games are read by different software and are not the same shelf, so one setting could not say which it meant. Without one configured, they default to `~/Videos`, `~/Music` and `~/Games`. A folder chosen while a CD is in the drive is a decision about music and leaves the other two alone.

**Preferred languages** decide what starts ticked. The rip page lists the languages the disc actually carries - taken from the scan, so there is nothing to spell - with the preferred ones ticked and moved to the top. Order is the order you switch them on, and the first one becomes the default track. A language you want that is not on the disc simply does not appear; a language on the disc that you have not asked for is still offered, just unticked.

**The MakeMKV fallback** can only be switched on if `makemkvcon` is installed. When it is missing the row is insensitive and says so, rather than being a live control whose promise would be broken forty minutes into a disc. Inside a flatpak the row is left out altogether, since MakeMKV is proprietary and can never be on `PATH` in there. The choice is honoured by both front ends through `rip::Auto`, so the window and the command line cannot drift apart about when MakeMKV gets involved.
