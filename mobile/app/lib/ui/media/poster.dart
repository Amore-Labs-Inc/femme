/// A still frame for a video attachment in the chat, as the desktop's ffmpeg poster
/// (`desktop/src/transcript/live.rs:615-639`): the platform player opens the file paused and
/// shows its first frame.
library;

import 'dart:async';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter/cupertino.dart';
import 'package:video_player/video_player.dart';

import '../theme.dart';

/// Opens a paused, muted controller on [bytes] (named [name], for its extension).
typedef PosterOpen = Future<VideoPlayerController> Function(
  Uint8List bytes,
  String name,
);

/// The poster controllers by blob key, least recently used first.
///
/// A player per video costs a decoder and a texture, so only a few are kept: 6 is more than a
/// screen of chat shows at once, and enough that scrolling back and forth does not open them
/// again. An entry a tile still shows is never closed; past the cap the oldest unused ones go.
/// A video that could not be opened (unsupported codec, no plugin) is remembered as null so
/// its tile stays the plain one without trying again.
class PosterCache {
  PosterCache({
    this.cap = 6,
    PosterOpen? open,
    void Function(VideoPlayerController)? close,
    bool? supported,
  }) : _open = open ?? _openFile,
       _close = close ?? _closeFile,
       supported = supported ?? _platformSupported();

  /// The one the chat uses.
  static final shared = PosterCache();

  final int cap;
  final PosterOpen _open;
  final void Function(VideoPlayerController) _close;

  /// False where video_player has no implementation (Linux desktop): tiles stay plain.
  final bool supported;

  final _entries = <String, _Entry>{}; // insertion-ordered: oldest first

  static bool _platformSupported() {
    try {
      return !Platform.isLinux;
    } catch (_) {
      return false;
    }
  }

  /// The keys held, least recently used first (for tests).
  @visibleForTesting
  List<String> get keys => _entries.keys.toList();

  /// The controller for [key] (opened from [bytes] the first time), or null when this video
  /// has no poster. Every [acquire] needs one [release].
  Future<VideoPlayerController?> acquire(
    String key,
    Uint8List bytes,
    String name,
  ) {
    if (!supported) return Future.value();
    var e = _entries.remove(key);
    e ??= _Entry(_load(bytes, name));
    e.users++;
    _entries[key] = e;
    _trim();
    return e.controller;
  }

  void release(String key) {
    final e = _entries[key];
    if (e == null || e.users == 0) return;
    e.users--;
    _trim();
  }

  Future<VideoPlayerController?> _load(Uint8List bytes, String name) async {
    try {
      return await _open(bytes, name);
    } catch (_) {
      // MissingPluginException (no implementation here) or a codec the player can't open.
      return null;
    }
  }

  void _trim() {
    if (_entries.length <= cap) return;
    final drop = <String>[];
    var over = _entries.length - cap;
    for (final MapEntry(:key, :value) in _entries.entries) {
      if (over == 0) break;
      if (value.users == 0) {
        drop.add(key);
        over--;
      }
    }
    for (final k in drop) {
      _entries.remove(k)!.controller.then((c) {
        if (c != null) _close(c);
      });
    }
  }

  static final _files = Expando<File>();

  static Future<VideoPlayerController> _openFile(
    Uint8List bytes,
    String name,
  ) async {
    final dot = name.lastIndexOf('.');
    final ext = dot < 0 ? '.mp4' : name.substring(dot).toLowerCase();
    final f = File(
      '${Directory.systemTemp.path}/femme-poster-'
      '${DateTime.now().microsecondsSinceEpoch}$ext',
    );
    await f.writeAsBytes(bytes, flush: true);
    final c = VideoPlayerController.file(f);
    _files[c] = f;
    try {
      await c.initialize();
      await c.setVolume(0);
      // Android shows nothing until a frame is decoded; a seek to the start draws the first.
      await c.seekTo(Duration.zero);
      return c;
    } catch (_) {
      _closeFile(c);
      rethrow;
    }
  }

  static void _closeFile(VideoPlayerController c) {
    final f = _files[c];
    unawaited(() async {
      try {
        await c.dispose();
      } catch (_) {}
      try {
        if (f != null && await f.exists()) await f.delete();
      } catch (_) {}
    }());
  }
}

class _Entry {
  _Entry(this.controller);
  final Future<VideoPlayerController?> controller;
  int users = 0;
}

/// [bytes]' first frame with a ▶ over it, or [fallback] while it opens or when it can't.
class VideoPoster extends StatefulWidget {
  const VideoPoster({
    super.key,
    required this.blobKey,
    required this.bytes,
    required this.name,
    required this.fallback,
    this.cache,
  });

  /// Identifies the video across rebuilds and chats (`sid\npath`).
  final String blobKey;
  final Uint8List bytes;
  final String name;
  final Widget fallback;
  final PosterCache? cache;

  @override
  State<VideoPoster> createState() => _VideoPosterState();
}

class _VideoPosterState extends State<VideoPoster> {
  late PosterCache _cache;
  late String _key;
  VideoPlayerController? _c;

  @override
  void initState() {
    super.initState();
    _get();
  }

  @override
  void didUpdateWidget(VideoPoster old) {
    super.didUpdateWidget(old);
    if (old.blobKey != widget.blobKey || old.cache != widget.cache) {
      _cache.release(_key);
      _c = null;
      _get();
    }
  }

  void _get() {
    _cache = widget.cache ?? PosterCache.shared;
    final key = _key = widget.blobKey;
    _cache.acquire(key, widget.bytes, widget.name).then((c) {
      if (mounted && _key == key && c != null) setState(() => _c = c);
    });
  }

  @override
  void dispose() {
    _cache.release(_key);
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final c = _c;
    if (c == null || !c.value.isInitialized) return widget.fallback;
    return ColoredBox(
      color: FemmeColors.ink,
      child: Stack(
        fit: StackFit.expand,
        children: [
          Center(
            child: AspectRatio(
              aspectRatio: c.value.aspectRatio,
              child: VideoPlayer(c),
            ),
          ),
          const Center(
            child: Icon(
              CupertinoIcons.play_circle_fill,
              color: FemmeColors.paper,
              size: 36,
            ),
          ),
        ],
      ),
    );
  }
}
