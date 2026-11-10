import 'dart:io';

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';
import 'package:video_player/video_player.dart';

import '../theme.dart';

/// The lightbox's backdrop.
const backdrop = Color(0xFF111111);

/// A picture fitted on a dark backdrop, pinch to zoom.
class ImageView extends StatelessWidget {
  const ImageView({super.key, required this.bytes});
  final Uint8List bytes;

  @override
  Widget build(BuildContext context) => ColoredBox(
    color: backdrop,
    child: InteractiveViewer(
      maxScale: 8,
      child: Center(
        child: Image.memory(
          bytes,
          fit: BoxFit.contain,
          gaplessPlayback: true,
          errorBuilder: (_, _, _) => const Text(
            'This picture could not be decoded.',
            style: TextStyle(color: FemmeColors.darkMuted),
          ),
        ),
      ),
    ),
  );
}

/// `1:05`, `1:02:03`.
String clock(Duration d) {
  final h = d.inHours;
  final m = d.inMinutes.remainder(60);
  final s = d.inSeconds.remainder(60).toString().padLeft(2, '0');
  return h > 0 ? '$h:${m.toString().padLeft(2, '0')}:$s' : '$m:$s';
}

/// A video or a sound from bytes: written to a temp file, played by the platform player, with
/// play/pause, time and a scrub track. [audio]: no picture, only the controls.
class MediaView extends StatefulWidget {
  const MediaView({
    super.key,
    required this.name,
    required this.bytes,
    this.audio = false,
  });

  final String name;
  final Uint8List bytes;
  final bool audio;

  @override
  State<MediaView> createState() => _MediaViewState();
}

class _MediaViewState extends State<MediaView> {
  VideoPlayerController? _c;
  Directory? _tmp;
  Object? _error;
  double? _scrub;

  @override
  void initState() {
    super.initState();
    _start();
  }

  Future<void> _start() async {
    try {
      final tmp = await Directory.systemTemp.createTemp('femme-files');
      _tmp = tmp;
      final file = File('${tmp.path}/${widget.name}');
      await file.writeAsBytes(widget.bytes, flush: true);
      final c = VideoPlayerController.file(file);
      await c.initialize();
      if (!mounted) {
        await c.dispose();
        return;
      }
      c.addListener(_tick);
      setState(() => _c = c);
    } catch (e) {
      if (mounted) setState(() => _error = e);
    }
  }

  void _tick() {
    if (mounted) setState(() {});
  }

  @override
  void dispose() {
    _c?.removeListener(_tick);
    _c?.dispose();
    final tmp = _tmp;
    if (tmp != null) tmp.delete(recursive: true).catchError((_) => tmp);
    super.dispose();
  }

  void _toggle() {
    final c = _c;
    if (c == null) return;
    HapticFeedback.selectionClick();
    if (c.value.isPlaying) {
      c.pause();
    } else {
      if (c.value.position >= c.value.duration) c.seekTo(Duration.zero);
      c.play();
    }
  }

  @override
  Widget build(BuildContext context) {
    const light = TextStyle(color: FemmeColors.darkInk, fontSize: 13);
    if (_error != null) {
      return ColoredBox(
        color: backdrop,
        child: Center(
          child: Padding(
            padding: const EdgeInsets.all(24),
            child: Text(
              'Could not play it: $_error',
              style: light,
              textAlign: TextAlign.center,
            ),
          ),
        ),
      );
    }
    final c = _c;
    if (c == null) {
      return const ColoredBox(
        color: backdrop,
        child: Center(
          child: CupertinoActivityIndicator(color: FemmeColors.darkInk),
        ),
      );
    }
    final v = c.value;
    final total = v.duration.inMilliseconds.toDouble();
    final at = _scrub ?? v.position.inMilliseconds.toDouble().clamp(0, total);
    return ColoredBox(
      color: backdrop,
      child: SafeArea(
        top: false,
        child: Column(
          children: [
            Expanded(
              child: Center(
                child: widget.audio || v.size.isEmpty
                    ? const Icon(
                        CupertinoIcons.music_note_2,
                        size: 96,
                        color: FemmeColors.darkMuted,
                      )
                    : GestureDetector(
                        onTap: _toggle,
                        child: AspectRatio(
                          aspectRatio: v.aspectRatio,
                          child: VideoPlayer(c),
                        ),
                      ),
              ),
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(8, 4, 16, 12),
              child: Row(
                children: [
                  CupertinoButton(
                    padding: const EdgeInsets.all(10),
                    minimumSize: const Size(44, 44),
                    onPressed: _toggle,
                    child: Icon(
                      v.isPlaying
                          ? CupertinoIcons.pause_fill
                          : CupertinoIcons.play_fill,
                      color: FemmeColors.darkInk,
                    ),
                  ),
                  Text(clock(Duration(milliseconds: at.round())), style: light),
                  Expanded(
                    child: CupertinoSlider(
                      value: total > 0 ? at.toDouble() : 0,
                      max: total > 0 ? total : 1,
                      activeColor: FemmeColors.accent,
                      thumbColor: FemmeColors.darkInk,
                      onChanged: total > 0
                          ? (x) => setState(() => _scrub = x)
                          : null,
                      onChangeEnd: (x) async {
                        await c.seekTo(Duration(milliseconds: x.round()));
                        if (mounted) setState(() => _scrub = null);
                      },
                    ),
                  ),
                  Text(clock(v.duration), style: light),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}
