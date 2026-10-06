import type { AnnotationObject } from "../types/domain";

export type AnnotationEditorState = {
  objects: AnnotationObject[];
  past: AnnotationObject[][];
  future: AnnotationObject[][];
  gestureBase: AnnotationObject[] | null;
};

type UpdateObjects = (objects: AnnotationObject[]) => AnnotationObject[];

export type AnnotationEditorAction =
  | { type: "load"; objects: AnnotationObject[] }
  | { type: "commit"; update: UpdateObjects }
  | { type: "begin-gesture" }
  | { type: "preview"; update: UpdateObjects }
  | { type: "end-gesture" }
  | { type: "undo" }
  | { type: "redo" };

export const initialAnnotationEditorState: AnnotationEditorState = {
  objects: [],
  past: [],
  future: [],
  gestureBase: null,
};

const historyLimit = 100;

function appendHistory(history: AnnotationObject[][], objects: AnnotationObject[]) {
  return [...history.slice(-(historyLimit - 1)), objects];
}

export function annotationEditorReducer(
  state: AnnotationEditorState,
  action: AnnotationEditorAction,
): AnnotationEditorState {
  switch (action.type) {
    case "load":
      return { objects: action.objects, past: [], future: [], gestureBase: null };
    case "commit": {
      const next = action.update(state.objects);
      if (next === state.objects) return state;
      return {
        objects: next,
        past: appendHistory(state.past, state.objects),
        future: [],
        gestureBase: null,
      };
    }
    case "begin-gesture":
      return state.gestureBase ? state : { ...state, gestureBase: state.objects };
    case "preview":
      return state.gestureBase ? { ...state, objects: action.update(state.objects) } : state;
    case "end-gesture": {
      if (!state.gestureBase) return state;
      if (JSON.stringify(state.gestureBase) === JSON.stringify(state.objects)) {
        return { ...state, gestureBase: null };
      }
      return {
        ...state,
        past: appendHistory(state.past, state.gestureBase),
        future: [],
        gestureBase: null,
      };
    }
    case "undo": {
      const previous = state.past[state.past.length - 1];
      if (!previous) return state;
      return {
        objects: previous,
        past: state.past.slice(0, -1),
        future: appendHistory(state.future, state.objects),
        gestureBase: null,
      };
    }
    case "redo": {
      const next = state.future[state.future.length - 1];
      if (!next) return state;
      return {
        objects: next,
        past: appendHistory(state.past, state.objects),
        future: state.future.slice(0, -1),
        gestureBase: null,
      };
    }
  }
}
