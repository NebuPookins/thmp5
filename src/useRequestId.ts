import { useCallback, useRef } from "react";

// Returns a stable function yielding a fresh, monotonically-increasing id on
// each call, for tagging in-flight async requests so a stale response can be
// dropped (see the reducers in `modalReducers.ts` / `entityDetailReducer.ts`).
export function useRequestId(): () => number {
  const ref = useRef(0);
  return useCallback(() => {
    ref.current += 1;
    return ref.current;
  }, []);
}
