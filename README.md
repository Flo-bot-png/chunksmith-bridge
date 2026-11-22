# Chunksmith Bridge

A client mod for [Sunrise](https://sunrise.so) that shows the distant terrain a
[Chunksmith](https://modrinth.com/mod/chunksmith) server hands out (made for Distant Horizons
and voxy) in Sunrise's own far terrain.

## How it works

1. When you join a server, the mod says hello on Chunksmith's `chunksmith:lod` channel
   (protocol 4, Chunksmith 4.x and newer).
2. It asks for the LOD regions around you and downloads them, over Chunksmith's download port
   or, on servers without one, over the game connection.
3. Every chunk is converted into Sunrise's far-terrain format and stored in
   `server-terrain/`, beside what Sunrise keeps of the chunks you saw yourself (those always win).

Block and biome numbers come from the game you're playing, so new blocks and biomes (and
those a server adds) get the right colours. Regions that would touch files Sunrise is using
are converted in the background while you play, once you're away from them, or when you leave
the server, or at the next start, before you join (the mod remembers the last session's block
and biome table for that). Progress is saved after every region, so whatever was downloaded
before is ready when you join. Terrain that comes in while you play shows after you rejoin.

`chunksmith-bridge/log.txt` tells you what's going on: `connected to Chunksmith`,
`fetching N far regions`, `N far regions stored`, `added N far chunks from N regions`.

## Requirements

- Sunrise's far terrain switched on: Options → Graphics → **LOD distance** above 0.
- A server running Chunksmith with LOD data. On Paper the plugin only serves over its download
  port (game port + 1 by default), so that port must be reachable.

## Settings

`chunksmith-bridge/settings.txt` in the game folder (create it if it isn't there):

```
radius=8192
chat=false
```

- `radius`: how far around you to fetch, in blocks (512 to 16384, default 8192). Every region
  of 512×512 blocks takes about 15 MB on disk: 8192 is up to ~12 GB, 16384 up to ~45 GB, if the
  server has that much.
- `chat`: `true` shows the mod's messages in the chat. They always go to
  `chunksmith-bridge/log.txt`.

## Tips and limits

- For finer far terrain when you zoom (spyglass or a zoom mod), switch on `lodZoomDetail` in
  Sunrise's extras.
- How fine far terrain looks (cells merge past ~1 km, no small plants, opaque far water) is up
  to Sunrise's renderer; the data this mod writes is accurate to the block.
- The block and biome table is built once per game session: after switching to a server with
  other biomes or blocks, restart the game.

## Licence

GPL-3.0-only, like Chunksmith, whose wire and file formats this mod follows.
