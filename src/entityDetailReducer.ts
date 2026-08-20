// State machine driving EntityDetailView's fetch of whichever entity is
// currently navigated to. The wire-model types live in `entityTypes.ts`.
//
// `artist` / `releaseGroup` / `recording` used to be three independently
// nullable useState fields, refreshed by an effect keyed on the `nav` prop.
// Nothing enforced that they stayed mutually exclusive, and navigating
// quickly (A -> B before A's fetch resolved) let A's stale response land
// after B was already showing — clobbering B's data and nulling it out.
// Collapsing them into one discriminated union with a requestId makes both
// bugs unrepresentable: only one entity can be "the" loaded entity at a
// time, and a stale response can't overwrite a newer one because its
// requestId no longer matches.

import type { ArtistDetail, ReleaseGroupDetail, RecordingDetail } from "./entityTypes";

export type DetailEntity =
  | { type: "artist"; data: ArtistDetail }
  | { type: "release_group"; data: ReleaseGroupDetail }
  | { type: "recording"; data: RecordingDetail };

export type DetailState =
  | { phase: "loading"; requestId: number }
  | { phase: "error"; requestId: number; error: string }
  | { phase: "ready"; requestId: number; entity: DetailEntity };

export type DetailAction =
  | { type: "load"; requestId: number }
  | { type: "loadOk"; requestId: number; entity: DetailEntity }
  | { type: "loadErr"; requestId: number; error: string };

export function detailReducer(state: DetailState, action: DetailAction): DetailState {
  switch (action.type) {
    case "load":
      return { phase: "loading", requestId: action.requestId };
    case "loadOk":
      if (state.requestId === action.requestId) {
        return { phase: "ready", requestId: state.requestId, entity: action.entity };
      }
      return state;
    case "loadErr":
      if (state.requestId === action.requestId) {
        return { phase: "error", requestId: state.requestId, error: action.error };
      }
      return state;
  }
}
