def shared_library_entries($prefix):
  [ .[]
    | select(.path | startswith($prefix))
    | .path = (.path | ltrimstr($prefix))
    | select(.path | test("^(lib[^/]+\\.so(\\..*)?|pulseaudio/lib[^/]+\\.so(\\..*)?)$"))
    | {path, kind, link_target: (.link_target // null)}
  ] | sort_by(.path);

def shared_library_owners($prefix):
  [ .[]
    | select(.path | startswith($prefix))
    | .path = (.path | ltrimstr($prefix))
    | select(.path | test("^(lib[^/]+\\.so(\\..*)?|pulseaudio/lib[^/]+\\.so(\\..*)?)$"))
    | .path |= sub("\\.so(\\..*)?$"; ".so")
    | {component, path}
  ] as $actual
  | if all($actual | group_by(.path)[]; (map(.component) | unique | length) == 1)
    then ($actual | unique_by(.path) | sort_by(.path))
    else error("shared-library aliases disagree on component owner")
    end;
