export type LoveToggleAction = {
  command: "lastfm_love_track" | "lastfm_unlove_track";
  nextLoved: boolean;
};

/** Decide which Last.fm command to invoke when the heart button is clicked, given the current loved state. */
export function loveToggleAction(currentlyLoved: boolean): LoveToggleAction {
  return currentlyLoved
    ? { command: "lastfm_unlove_track", nextLoved: false }
    : { command: "lastfm_love_track", nextLoved: true };
}
