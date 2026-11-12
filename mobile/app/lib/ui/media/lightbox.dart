/// The full-screen viewer for a picture or a video the agent showed or a file the user opened:
/// pinch-zoom pictures, play videos, share either, close with ✕ or a swipe down.
library;

import 'dart:async';
import 'dart:io';

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:video_player/video_player.dart';

import '../theme.dart';
import 'actions.dart';

/// Opens [bytes] (named [name]) full screen; a video when [video]. Dark whatever the theme.
Future<void> showLightbox(
  BuildContext context, {
  required Uint8List bytes,
  required String name,
  bool video = false,
}) {
  return Navigator.of(context, rootNavigator: true).push<void>(
    PageRouteBuilder<void>(
      opaque: false,
      barrierColor: null,
      transitionDuration: const Duration(milliseconds: 180),
      reverseTransitionDuration: const Duration(milliseconds: 180),
      pageBuilder: (_, _, _) =>
          _Lightbox(bytes: bytes, name: name, video: video),
      transitionsBuilder: (_, anim, _, child) => FadeTransition(
        opacity: CurvedAnimation(parent: anim, curve: _curve),
        child: child,
      ),
    ),
  );
}

const _curve = Cubic(.32, .72, 0, 1);
const _backdrop = FemmeColors.darkPaper;
const _fg = FemmeColors.darkInk;
const _dim = FemmeColors.darkMuted;

String _ext(String name) {
  final dot = name.lastIndexOf('.');
  if (dot < 0 || dot < name.lastIndexOf('/')) return '';
  return name.substring(dot).toLowerCase();
}

String _clock(Duration d) {
  final s = d.inSeconds;
  final h = s ~/ 3600;
  final m = (s % 3600) ~/ 60;
  final ss = (s % 60).toString().padLeft(2, '0');
  return h > 0 ? '$h:${m.toString().padLeft(2, '0')}:$ss' : '$m:$ss';
}

class _Lightbox extends StatefulWidget {
  const _Lightbox({
    required this.bytes,
    required this.name,
    required this.video,
  });
  final Uint8List bytes;
  final String name;
  final bool video;

  @override
  State<_Lightbox> createState() => _LightboxState();
}

class _LightboxState extends State<_Lightbox>
    with SingleTickerProviderStateMixin {
  final _zoom = TransformationController();
  final _shareKey = GlobalKey();
  late final AnimationController _back = AnimationController(
    vsync: this,
    duration: const Duration(milliseconds: 180),
  );
  Animation<double>? _backAnim;
  double _dy = 0;
  bool _dragging = false;
  int _pointers = 0;
  bool _chrome = true;
  Offset? _tapAt;

  bool get _zoomed => _zoom.value.getMaxScaleOnAxis() > 1.01;
  bool get _svg => _ext(widget.name) == '.svg';

  @override
  void initState() {
    super.initState();
    _zoom.addListener(_onZoom);
    _back.addListener(() {
      final a = _backAnim;
      if (a != null) setState(() => _dy = a.value);
    });
  }

  bool _wasZoomed = false;
  void _onZoom() {
    final z = _zoomed;
    if (z != _wasZoomed) setState(() => _wasZoomed = z);
  }

  @override
  void dispose() {
    _zoom.dispose();
    _back.dispose();
    super.dispose();
  }

  // ---- swipe down to close ----

  void _dragStart(DragStartDetails _) {
    if (_zoomed || _pointers > 1) return;
    _back.stop();
    _dragging = true;
  }

  void _dragUpdate(DragUpdateDetails d) {
    if (!_dragging) return;
    if (_pointers > 1) {
      _springBack();
      return;
    }
    setState(() => _dy = (_dy + d.delta.dy).clamp(-200.0, double.infinity));
  }

  void _dragEnd(DragEndDetails d) {
    if (!_dragging) return;
    _dragging = false;
    final v = d.velocity.pixelsPerSecond.dy;
    if (_dy > 120 || (v > 900 && _dy > 0)) {
      Navigator.of(context).pop();
    } else {
      _springBack();
    }
  }

  void _springBack() {
    _dragging = false;
    _backAnim = Tween<double>(
      begin: _dy,
      end: 0,
    ).animate(CurvedAnimation(parent: _back, curve: _curve));
    _back.forward(from: 0);
  }

  // ---- zoom ----

  void _doubleTap() {
    if (_zoomed) {
      _zoom.value = Matrix4.identity();
      return;
    }
    final p = _tapAt ?? Offset.zero;
    const s = 2.5;
    _zoom.value = Matrix4.identity()
      ..translateByDouble(-p.dx * (s - 1), -p.dy * (s - 1), 0, 1)
      ..scaleByDouble(s, s, 1, 1);
  }

  // ---- share ----

  Future<void> _share() async {
    HapticFeedback.lightImpact();
    Rect? origin;
    final box = _shareKey.currentContext?.findRenderObject();
    if (box is RenderBox && box.hasSize) {
      origin = box.localToGlobal(Offset.zero) & box.size;
    }
    await shareBytes(widget.bytes, widget.name, origin: origin);
  }

  // ---- build ----

  Widget _content() {
    if (widget.video) return _VideoView(bytes: widget.bytes, name: widget.name);
    return GestureDetector(
      onTap: () => setState(() => _chrome = !_chrome),
      onDoubleTapDown: (d) => _tapAt = d.localPosition,
      onDoubleTap: _doubleTap,
      child: InteractiveViewer(
        transformationController: _zoom,
        minScale: 1,
        maxScale: 6,
        panEnabled: _wasZoomed,
        child: SizedBox.expand(
          child: _svg
              ? SvgPicture.memory(
                  widget.bytes,
                  fit: BoxFit.contain,
                  placeholderBuilder: (_) => const Center(
                    child: CupertinoActivityIndicator(color: _fg),
                  ),
                  errorBuilder: (_, _, _) => const _Note(
                    icon: CupertinoIcons.exclamationmark_triangle,
                    text: "can't open this svg; share it to open it elsewhere",
                  ),
                )
              : Image.memory(
                  widget.bytes,
                  fit: BoxFit.contain,
                  gaplessPlayback: true,
                  errorBuilder: (_, _, _) => const _Note(
                    icon: CupertinoIcons.exclamationmark_triangle,
                    text: "can't open this picture",
                  ),
                ),
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final fade = (1 - (_dy.abs() / 400)).clamp(0.3, 1.0);
    final pad = MediaQuery.paddingOf(context);
    return Stack(
      fit: StackFit.expand,
      children: [
        ColoredBox(color: _backdrop.withValues(alpha: fade)),
        Listener(
          onPointerDown: (_) => _pointers++,
          onPointerUp: (_) => _pointers = (_pointers - 1).clamp(0, 99),
          onPointerCancel: (_) => _pointers = (_pointers - 1).clamp(0, 99),
          child: GestureDetector(
            onVerticalDragStart: _wasZoomed ? null : _dragStart,
            onVerticalDragUpdate: _wasZoomed ? null : _dragUpdate,
            onVerticalDragEnd: _wasZoomed ? null : _dragEnd,
            onVerticalDragCancel: _wasZoomed ? null : _springBack,
            child: Transform.translate(
              offset: Offset(0, _dy),
              child: Padding(
                padding: EdgeInsets.only(top: pad.top, bottom: pad.bottom),
                child: _content(),
              ),
            ),
          ),
        ),
        AnimatedOpacity(
          opacity: _chrome && _dy.abs() < 8 ? 1 : 0,
          duration: const Duration(milliseconds: 180),
          child: IgnorePointer(
            ignoring: !_chrome,
            child: _TopBar(
              name: widget.name,
              shareKey: _shareKey,
              onClose: () => Navigator.of(context).pop(),
              onShare: _share,
            ),
          ),
        ),
      ],
    );
  }
}

class _TopBar extends StatelessWidget {
  const _TopBar({
    required this.name,
    required this.shareKey,
    required this.onClose,
    required this.onShare,
  });
  final String name;
  final Key shareKey;
  final VoidCallback onClose;
  final VoidCallback onShare;

  @override
  Widget build(BuildContext context) {
    return Align(
      alignment: Alignment.topCenter,
      child: Container(
        color: _backdrop.withValues(alpha: 0.72),
        padding: EdgeInsets.only(
          top: MediaQuery.paddingOf(context).top,
          left: MediaQuery.paddingOf(context).left + 4,
          right: MediaQuery.paddingOf(context).right + 4,
        ),
        child: SizedBox(
          height: 48,
          child: Row(
            children: [
              _IconButton(
                icon: CupertinoIcons.xmark,
                label: 'Close',
                onTap: onClose,
              ),
              Expanded(
                child: Text(
                  name,
                  textAlign: TextAlign.center,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: const TextStyle(
                    color: _fg,
                    fontSize: 15,
                    fontWeight: FontWeight.w600,
                    decoration: TextDecoration.none,
                  ),
                ),
              ),
              _IconButton(
                key: shareKey,
                icon: CupertinoIcons.share,
                label: 'Share',
                onTap: onShare,
              ),
            ],
          ),
        ),
      ),
    );
  }
}

class _IconButton extends StatelessWidget {
  const _IconButton({
    super.key,
    required this.icon,
    required this.label,
    required this.onTap,
  });
  final IconData icon;
  final String label;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) => Semantics(
    button: true,
    label: label,
    child: CupertinoButton(
      padding: EdgeInsets.zero,
      minimumSize: const Size(44, 44),
      onPressed: onTap,
      child: Icon(icon, color: _fg, size: 24),
    ),
  );
}

class _Note extends StatelessWidget {
  const _Note({required this.icon, required this.text});
  final IconData icon;
  final String text;

  @override
  Widget build(BuildContext context) => Center(
    child: Padding(
      padding: const EdgeInsets.all(24),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Icon(icon, color: _dim, size: 40),
          const SizedBox(height: 12),
          Text(
            text,
            textAlign: TextAlign.center,
            style: const TextStyle(
              color: _dim,
              fontSize: 15,
              decoration: TextDecoration.none,
            ),
          ),
        ],
      ),
    ),
  );
}

// ---- video ----

/// Sound off in the lightbox's videos; the choice holds for the rest of the app run.
bool _muted = false;

@visibleForTesting
bool get lightboxMuted => _muted;
@visibleForTesting
set lightboxMuted(bool v) => _muted = v;

/// Makes the player for a video's temp file; tests hand a fake.
@visibleForTesting
VideoPlayerController Function(File file) lightboxPlayer =
    VideoPlayerController.file;

/// Plays the bytes from a temp file (video_player cannot play from memory); the file goes
/// with the view.
class _VideoView extends StatefulWidget {
  const _VideoView({required this.bytes, required this.name});
  final Uint8List bytes;
  final String name;

  @override
  State<_VideoView> createState() => _VideoViewState();
}

class _VideoViewState extends State<_VideoView> {
  VideoPlayerController? _c;
  File? _file;
  String? _error;
  bool _disposed = false;

  /// While the user scrubs, the slider shows this instead of the player's position.
  double? _scrubMs;

  @override
  void initState() {
    super.initState();
    _open();
  }

  Future<void> _open() async {
    try {
      var ext = _ext(widget.name);
      if (ext.isEmpty) ext = '.mp4';
      final f = File(
        '${Directory.systemTemp.path}/femme-lightbox-'
        '${DateTime.now().microsecondsSinceEpoch}$ext',
      );
      await f.writeAsBytes(widget.bytes, flush: true);
      _file = f;
      if (_disposed) {
        _cleanup();
        return;
      }
      final c = lightboxPlayer(f);
      _c = c;
      await c.initialize();
      if (_disposed) return;
      c.addListener(_tick);
      await c.setVolume(_muted ? 0 : 1);
      await c.play();
      if (mounted) setState(() {});
    } catch (e) {
      if (mounted) setState(() => _error = "can't play this video: $e");
    }
  }

  void _tick() {
    if (mounted) setState(() {});
  }

  void _cleanup() {
    final c = _c;
    _c = null;
    c?.removeListener(_tick);
    final f = _file;
    _file = null;
    unawaited(() async {
      try {
        await c?.dispose();
      } catch (_) {}
      try {
        if (f != null && await f.exists()) await f.delete();
      } catch (_) {}
    }());
  }

  @override
  void dispose() {
    _disposed = true;
    _cleanup();
    super.dispose();
  }

  void _toggle() {
    final c = _c;
    if (c == null || !c.value.isInitialized) return;
    if (c.value.isPlaying) {
      c.pause();
    } else {
      if (c.value.position >= c.value.duration) c.seekTo(Duration.zero);
      c.play();
    }
  }

  void _toggleSound() {
    HapticFeedback.selectionClick();
    setState(() => _muted = !_muted);
    _c?.setVolume(_muted ? 0 : 1);
  }

  @override
  Widget build(BuildContext context) {
    if (_error != null) {
      return _Note(
        icon: CupertinoIcons.exclamationmark_triangle,
        text: _error!,
      );
    }
    final c = _c;
    if (c == null || !c.value.isInitialized) {
      return const Center(child: CupertinoActivityIndicator(color: _fg));
    }
    final v = c.value;
    if (v.hasError) {
      return _Note(
        icon: CupertinoIcons.exclamationmark_triangle,
        text: "can't play this video: ${v.errorDescription ?? 'unknown error'}",
      );
    }
    final totalMs = v.duration.inMilliseconds.toDouble();
    final posMs = (_scrubMs ?? v.position.inMilliseconds.toDouble()).clamp(
      0.0,
      totalMs <= 0 ? 0.0 : totalMs,
    );
    return Column(
      children: [
        const SizedBox(height: 48),
        Expanded(
          child: GestureDetector(
            onTap: _toggle,
            child: Center(
              child: AspectRatio(
                aspectRatio: v.aspectRatio <= 0 ? 16 / 9 : v.aspectRatio,
                child: VideoPlayer(c),
              ),
            ),
          ),
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(8, 8, 8, 8),
          child: Row(
            children: [
              _IconButton(
                icon: v.isPlaying
                    ? CupertinoIcons.pause_fill
                    : CupertinoIcons.play_fill,
                label: v.isPlaying ? 'Pause' : 'Play',
                onTap: _toggle,
              ),
              Text(
                _clock(Duration(milliseconds: posMs.round())),
                style: const TextStyle(
                  color: _fg,
                  fontSize: 13,
                  fontFeatures: [FontFeature.tabularFigures()],
                  decoration: TextDecoration.none,
                ),
              ),
              Expanded(
                child: CupertinoSlider(
                  value: posMs,
                  max: totalMs <= 0 ? 1 : totalMs,
                  activeColor: FemmeColors.accent,
                  thumbColor: _fg,
                  onChangeStart: (x) => setState(() => _scrubMs = x),
                  onChanged: totalMs <= 0
                      ? null
                      : (x) {
                          setState(() => _scrubMs = x);
                          c.seekTo(Duration(milliseconds: x.round()));
                        },
                  onChangeEnd: (x) async {
                    await c.seekTo(Duration(milliseconds: x.round()));
                    if (mounted) setState(() => _scrubMs = null);
                  },
                ),
              ),
              Text(
                _clock(v.duration),
                style: const TextStyle(
                  color: _dim,
                  fontSize: 13,
                  fontFeatures: [FontFeature.tabularFigures()],
                  decoration: TextDecoration.none,
                ),
              ),
              _IconButton(
                icon: _muted
                    ? CupertinoIcons.speaker_slash_fill
                    : CupertinoIcons.speaker_2_fill,
                label: _muted ? 'Sound on' : 'Mute',
                onTap: _toggleSound,
              ),
            ],
          ),
        ),
      ],
    );
  }
}
