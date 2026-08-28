# Working out what a disc is

## Where the metadata comes from

Three catalogues, asked in order until one answers - not merged, because two that both know a show would offer it twice.

| | television | film | key |
|---|---|---|---|
| TMDB | yes | yes | **needed** |
| TVmaze | yes | no | no |
| Wikidata | no | yes | no |

TMDB goes first when `TMDB_API_KEY` is set: it is the better data, it covers both, and it is what Jellyfin will consult about the same files afterwards - Jellyfin simply ships a key of its own, which is why it never asks for one. Without a key, TVmaze answers for television and Wikidata for film, so nothing needs a key for anything.

Ids carry their origin - `tvmaze:1633`, `wikidata:Q337078` - and are only ever handed back to whoever minted them. TVmaze's 1633 and TMDB's 1633 are different shows, and before this the episode list for one could be fetched for the other.

**Wikidata is the film answer because a film needs so little.** For a series the catalogue is load-bearing: without episode titles and numbers nothing can be named. A film needs a title, a year, and - most usefully - a runtime, which Wikidata carries as P2047. That runtime is evidence rather than description: a search ranks by how well the label matches, not by how well known the work is, so `The Big Lebowski: A XXX Parody` comes back beside the film it parodies, and 117 minutes against 155 is what separates them.


## What to take off a disc

A season disc carries perhaps seven episodes and thirty pieces of bonus material, so what you skip is most of the reading as well as most of the files.

| | |
|---|---|
| episodes, or the feature | always |
| play-alls | never - the same video a second time |
| extended cuts | `--no-extended` to skip |
| bonus material | `--no-extras` to skip |

Unticked means *not read*, not read-then-discarded. The one subtlety: an extended cut cannot be told from an ordinary extra before the file exists, since spotting one means comparing pictures. So an episode-length title nobody claimed is read if either switch wants it, and only then compared.


## Identification

A DVD carries no usable identifier, and there is no database keyed by disc. Redump and the DVD-Video hash registries cover games and preservation, not retail television, and neither can answer "which episodes are on this disc". So two independent kinds of evidence are combined, and both are shown:

- **The volume label.** `PARKS_AND_RECREATION_S7D1` is the single most informative thing on a disc and it costs nothing to read. It is also capped at 32 characters, so it truncates, and every authoring house has its own conventions - it can only ever be a hypothesis to search with.
- **The disc's own structure.** How many episode-length titles there are, how long they run, and how they group under the "play all" title. A play-all replays the episodes back to back, so its chapter list is theirs concatenated - decomposing it recovers both which titles are episodes and what order they belong in, with no network and no guessing.

A candidate is only trusted when the two agree, and the reasons are carried along so a wrong guess is visible rather than mysterious. Which disc of a season you are holding is genuinely ambiguous from one disc - a season split 5/5/4 and one split 4/4/6 look identical from disc two - so episode numbering prefers what is already in the output folder, then falls back to a guess it tells you about.
