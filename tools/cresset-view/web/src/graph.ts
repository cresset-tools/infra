// The revision topology layout, kept apart from the viewer so it can be exercised without a DOM.
//
// This is the one piece of the paged revision list with no cheap way to eyeball it: the layout
// carries lane state from one page into the next, and a mistake there does not throw — it draws
// a subtly wrong history. `graph.test.ts` pins the property that matters: laying out a history
// in pages must produce exactly what laying it out in one go produces.

/// Just the fields the layout reads. Keeps the test from having to build whole revisions.
export interface GraphCommit {
  commit_id: string;
  parent_commit_ids: string[];
}

export interface GraphSegment {
  fromLane: number;
  toLane: number;
  fromY: number;
  toY: number;
  colorLane: number;
}

export interface GraphRow {
  lane: number;
  laneCount: number;
  segments: GraphSegment[];
}

/// Lay out the topology for one page of revisions.
///
/// `lanes` is the carried state: the layout is a single top-down pass where each row depends
/// only on the lane assignment left by the row above, so a page can continue from where the
/// previous one stopped. Re-running this over the whole loaded list on every page would be
/// both wasteful and wrong — it would reassign lanes under rows already on screen.
export function layoutRevisionGraph(revisions: GraphCommit[], lanes: Array<string | null>): GraphRow[] {
  const rows: GraphRow[] = [];

  for (const revision of revisions) {
    let lane = lanes.indexOf(revision.commit_id);
    const continuesFromAbove = lane >= 0;
    if (lane < 0) {
      lane = lanes.indexOf(null);
      if (lane < 0) lane = lanes.length;
      lanes[lane] = revision.commit_id;
    }

    const segments: GraphSegment[] = [];
    for (let activeLane = 0; activeLane < lanes.length; activeLane += 1) {
      if (activeLane !== lane && lanes[activeLane] != null) {
        segments.push({ fromLane: activeLane, toLane: activeLane, fromY: 0, toY: 100, colorLane: activeLane });
      }
    }
    if (continuesFromAbove) {
      segments.push({ fromLane: lane, toLane: lane, fromY: 0, toY: 50, colorLane: lane });
    }

    lanes[lane] = null;
    for (const [parentIndex, parentId] of revision.parent_commit_ids.entries()) {
      let parentLane = lanes.indexOf(parentId);
      if (parentLane < 0) {
        parentLane = parentIndex === 0 && lanes[lane] == null ? lane : lanes.indexOf(null);
        if (parentLane < 0) parentLane = lanes.length;
        lanes[parentLane] = parentId;
      }
      segments.push({ fromLane: lane, toLane: parentLane, fromY: 50, toY: 100, colorLane: parentLane });
    }

    while (lanes.length > 0 && lanes.at(-1) == null) lanes.pop();
    rows.push({ lane, laneCount: Math.max(lane + 1, lanes.length), segments });
  }

  return rows;
}

export function graphLaneX(lane: number, laneGap: number): number {
  return 12 + lane * laneGap;
}

