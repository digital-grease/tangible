# Print an image's layout as libmirage reads it: one line per track.
import sys
import gi
gi.require_version("Mirage", "3.2")
from gi.repository import Mirage

Mirage.initialize()
for path in sys.argv[1:]:
    context = Mirage.Context()
    try:
        disc = context.load_image([path])
    except Exception as error:  # noqa: BLE001
        print(f"{path}: refused: {error}")
        continue
    name = path.rsplit("/", 1)[-1]
    print(f"{name}: medium {disc.get_medium_type().value_nick} sessions {disc.get_number_of_sessions()}")
    for s in range(disc.get_number_of_sessions()):
        session = disc.get_session_by_index(s)
        for t in range(session.get_number_of_tracks()):
            track = session.get_track_by_index(t)
            frags = []
            for f in range(track.get_number_of_fragments()):
                frag = track.get_fragment_by_index(f)
                frags.append(
                    f"len={frag.get_length()} off={frag.main_data_get_offset()} "
                    f"size={frag.main_data_get_size()} sub={frag.subchannel_data_get_size()}"
                )
            print(
                f"  session {session.layout_get_session_number()} track {track.layout_get_track_number()} "
                f"type {track.get_sector_type().value_nick} start {track.layout_get_start_sector()} "
                f"length {track.layout_get_length()} pregap {track.get_track_start()} "
                f"ctl {track.get_ctl()} | " + " ; ".join(frags)
            )
