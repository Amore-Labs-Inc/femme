/// The `Attached: <path>` convention (BODIES.md §6): lines the daemon appends for uploaded or
/// shown files, removed from the text and drawn as thumbnails or chips.
library;

const _pictureExt = {'png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'svg'};
const _videoExt = {'mp4', 'mov', 'webm', 'mkv', 'avi', 'm4v'};

/// A message's text without its `Attached:` lines (trailing space trimmed), and their paths
/// in order.
({String text, List<String> paths}) splitAttached(String text) {
  const tag = 'Attached: ';
  final kept = <String>[];
  final paths = <String>[];
  for (final line in text.split('\n')) {
    if (line.startsWith(tag)) {
      final p = line.substring(tag.length).trim();
      if (p.isNotEmpty) {
        paths.add(p);
        continue;
      }
    }
    kept.add(line);
  }
  return (text: kept.join('\n').trimRight(), paths: paths);
}

String fileName(String path) {
  final i = path.lastIndexOf('/');
  return i < 0 ? path : path.substring(i + 1);
}

String _ext(String path) {
  final n = fileName(path);
  final i = n.lastIndexOf('.');
  return i < 0 ? '' : n.substring(i + 1).toLowerCase();
}

bool isPicture(String path) => _pictureExt.contains(_ext(path));
bool isVideo(String path) => _videoExt.contains(_ext(path));
bool isSvg(String path) => _ext(path) == 'svg';

/// Claude's compaction hand-off arrives as a user message starting with this.
const compactionPrefix =
    'This session is being continued from a previous conversation';
