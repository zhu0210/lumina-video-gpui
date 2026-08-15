def valid_shared_library_allowlist:
  .components as $components
  | .audit.shared_library_allowlist as $items
  | ($items | type == "array" and length > 0)
    and all($items[];
      (.component | type == "string" and length > 0)
      and (.path | type == "string" and test("^(lib[A-Za-z0-9_.+-]+\\.so|pulseaudio/lib[A-Za-z0-9_.+-]+\\.so)$")))
    and (($items | map(.path) | length) == ($items | map(.path) | unique | length))
    and all($items[]; .component as $owner | any($components[]; .name == $owner));

def shared_library_entries($prefix):
  [ .[]
    | select(.path | startswith($prefix))
    | .path = (.path | ltrimstr($prefix))
    | select(.path | test("^(lib[A-Za-z0-9_.+-]+\\.so|pulseaudio/lib[A-Za-z0-9_.+-]+\\.so)(\\.[0-9]+){0,3}$"))
    | {path, kind, link_target: (.link_target // null)}
  ] | sort_by(.path);

def shared_library_owners($prefix):
  [ .[]
    | select(.path | startswith($prefix))
    | .path = (.path | ltrimstr($prefix))
    | select(.path | test("^(lib[A-Za-z0-9_.+-]+\\.so|pulseaudio/lib[A-Za-z0-9_.+-]+\\.so)(\\.[0-9]+){0,3}$"))
    | .path |= sub("\\.so(\\.[0-9]+){0,3}$"; ".so")
    | {component, path}
  ] as $actual
  | if all($actual | group_by(.path)[]; (map(.component) | unique | length) == 1)
    then ($actual | unique_by(.path) | sort_by(.path))
    else error("shared-library aliases disagree on component owner")
    end;
