/**
 * Whether a `player-track-ended` event concerns the track currently shown.
 *
 * `knownSourceIds` are the ids the UI associates with the current track: the
 * queued item's source and the source the backend reported starting for it
 * (they differ when a missing file fell back to another copy). The backend may send an empty source id (when it could not
 * read it); such an event is accepted. Otherwise an id matching none of them
 * means the event is stale: a newer track has already replaced the one that ended.
 */
export function endedEventAppliesTo(
  eventSourceId: string,
  knownSourceIds: ReadonlyArray<string | null | undefined>,
): boolean {
  return eventSourceId === "" || knownSourceIds.includes(eventSourceId);
}
