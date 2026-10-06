# Guild Wars Navigator

A visualizer of the Guild Wars Pathing data in Rust. Also used for waypointing and analyzing maps.

## Goals

Goals are in required implementation order.

Current status:

| Goal                  | Status        |
|-----------------------|---------------|
| MapDb                 | Completed     |
| FileConn              | In Progress   |
| Visualizer            | TBD           |
| Waypointer Tool       | TBD           |
| Pathfinding/analysis  | TBD           |

### 1. MapDb

- Open an sqlite database that holds a table of map_zones along with their associated mapfileids.
- Ability to import and merge rows from another sqlite db with the same table. This way we can import found mapfileids from other tools.
- Will be used to render a list of maps for map selection, searching and filtering.

Use this as the schema for MapDb, same as GWBS maploadlog.lua. SQL table of mapfiles.db:

```sql
CREATE TABLE IF NOT EXISTS map_zones (
    mapid INTEGER NOT NULL,
    instance TEXT NOT NULL CHECK (instance IN ('outpost', 'explorable')),
    name TEXT,
    mapfile INTEGER,
    unknown INTEGER,
    PRIMARY KEY (mapid, instance)
);
```

### 2. FileConn

- Ability to download the mapfile the pathing maps from the guild wars fileserver on demand if needed.
- You can download the map blob keyed by the mapfileid given.
- map blob is a FFNA blob as roughly defined in `references/pathingmaps/Pathingmap Builder/FFNA.h`
- Parse the pathing data from the downloaded mapfile since the mapfile blob carries more data.
  - Cache the pathing data keyed by the mapfileid.
  - Will likely need to put in work on reverse-engineering the file format.
- Reference `references/file` for python code that crudely implements the fileserver connection/download protocol.
- Reference `references/pathingmaps` for some code designed to parse pathing data from the DAT file.
  - Unconfirmed if the raw format from the fileserver 1:1 matches DATfile format.

### 3. Visualizer

- Implement a wgpu-based 2d pathing visualizer UI.
- Ability to target...
  - a windows + linux desktop app via winit
  - web-based rendering via wasm and webgl, webgpu if needed
- Implement egui window with a table representation of MapDb table.
  - Selecting a table row loads that map into the visualizer.
    - If pmap data is not cached locally, use FileConn to download and process the map blob into the pathing data. Essentially acts as a lazy load of mapfiles on-demand.
  - Add option to manually type a mapfileid to download/load into the visualizer.

### 4. Waypointer Tool

- Waypoint string format (using c format style since im not fully familiar with rust style):

  ```c
  { x = %5.5f, y = %5.5f, plane = %d },\n
  ```

- Allow right-click of point in pmap to register a waypoint.
- Render waypoints in visualizer as circle points with lines linking waypoints in order.
- egui window showing waypoint list along with other waypointer UI.
  - each waypoint in list has delete/trash button to remove waypoint from list.
- Allow left-click drag of a waypoint to edit its position.
- Remove waypoint if delete key is hit while hovering a point in visualizer.
- button on egui window to clear waypoint list.
- button on egui window to put string formatted waypoint list on to clipboard.
- button on egui window to read string formatted waypoint list from clipboard into live waypoint list.

### 5. Pathfinding and map analysis

TBD
