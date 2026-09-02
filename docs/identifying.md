# Working out what a disc is

This is about video. A music CD is named from the disc id its table of contents hashes to, and a game disc from what its dump hashes to against a Redump datfile, both of which are exact where this is inference. Video is the only one of the three that has to be argued for.

## Where the metadata comes from

Three catalogues, asked in order until one answers - not merged, because two that both know a show would offer it twice.

| | television | film | key |
|---|---|---|---|
| TMDB | yes | yes | **needed** |
| TVmaze | yes | no | no |
| Wikidata | no | yes | no |

TMDB goes first when `TMDB_API_KEY` is set: it is the better data, it covers both, and it is what Jellyfin will consult about the same files afterwards - Jellyfin simply ships a key of its own, which is why it never asks for one. Without a key, TVmaze answers for television and Wikidata for film, so nothing needs a key for anything.

Ids carry their origin - `tvmaze:1633`, `wikidata:Q337078` - and are handed back to whoever minted them, never to another catalogue. TVmaze's 1633 and TMDB's 1633 are different shows, and before this the episode list for one could be fetched for the other.

**The poster comes from Wikipedia, not from Wikidata.** Wikidata names images by their file on Commons, and a film poster is copyrighted, so Commons cannot hold one and Wikidata's "film poster" property is empty for practically every film there is - Star Wars and The Matrix both have nothing in it. What they do have is a logo, so every film came back wearing its wordmark. Wikipedia hosts the poster itself under fair use, and its article images can be asked for by title: one request covers every candidate, and `pilicense=any` is what makes it answer at all, since the default excludes exactly the non-free files a poster always is.

Which article to ask about comes from the Wikidata item's own link to it, never from the film's name. "Cloudy with a Chance of Meatballs" is a picture book; the film is filed under "Cloudy with a Chance of Meatballs (film)", and the item knows that where a title does not. The logo is still there as a last resort - it is not a poster, but it is the film's own mark and beats a generic icon.

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

A DVD carries no usable identifier, and there is no database keyed by disc. Redump and the DVD-Video hash registries cover games and preservation, not retail television, and neither can answer "which episodes are on this disc". So independent kinds of evidence are combined, and all of them are shown:

- **The volume label.** `PARKS_AND_RECREATION_S7D1` is the single most informative thing on a disc and it costs nothing to read. It is also capped at 32 characters, so it truncates, and every authoring house has its own conventions - it is a hypothesis to search with rather than an answer.
- **The disc's own structure.** How many episode-length titles there are, how long they run, and how they group under the "play all" title. A play-all replays the episodes back to back, so its chapter list is theirs concatenated - decomposing it recovers both which titles are episodes and what order they belong in, with no network and no guessing.

Structure speaks for a film as well as for a season. One title far longer than an episode, with no run of episode-length titles beside it, is a film and is not a season, and both halves of that are scored. Before they were, every piece of evidence past the name spoke only for television: a series could climb from a name match towards certainty on episode counts and runtimes agreeing, while a film had nothing it could add, so any show sharing a name outranked the film in the drive. Kung Fu Panda came back as a twenty-six episode series.

A year on the label is used where there is one, and only to prefer a candidate whose year agrees. It never argues against one, and the title keeps the number, since stripping it would search for "Blade Runner" and find the 1982 film, while "2012" and "Apollo 13" would otherwise contradict every correct answer.

A candidate is only trusted when the evidence agrees, and the reasons are carried along so a wrong guess is visible rather than mysterious. Which disc of a season you are holding cannot be told from one disc alone - a season split 5/5/4 and one split 4/4/6 look identical from disc two - so episode numbering prefers what is already in the output folder, then falls back to a guess it tells you about.
