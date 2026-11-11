/// The chat's controller handed down the rows, and the `Attached:` thumbnails and chips.
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';
import 'package:flutter_svg/flutter_svg.dart';

import '../../state/chat.dart';
import '../media/actions.dart';
import '../media/lightbox.dart';
import '../media/poster.dart';
import '../theme.dart';
import 'attached.dart';
import 'look.dart';

/// Gives rows the [ChatController]; they rebuild when it changes.
class ChatScope extends InheritedNotifier<ChatController> {
  const ChatScope({
    super.key,
    required ChatController controller,
    required super.child,
  }) : super(notifier: controller);

  static ChatController of(BuildContext context) =>
      context.dependOnInheritedWidgetOfExactType<ChatScope>()!.notifier!;
}

/// Opens a file from the box in the lightbox (fetched with `FsRead` if not cached).
Future<void> openAttached(BuildContext context, String path) async {
  final c = ChatScope.of(context);
  try {
    final bytes = await c.readFile(path);
    if (!context.mounted) return;
    await showLightbox(
      context,
      bytes: bytes,
      name: fileName(path),
      video: isVideo(path),
    );
  } catch (e) {
    if (context.mounted) {
      toast(context, "can't open ${fileName(path)}: ${errorText(e)}");
    }
  }
}

/// The long-press menu of a thumbnail or video tile: Open, Share, Save to Photos (not for an
/// SVG, which Photos can't take), Copy image (a raster picture, where the platform can).
Future<void> attachedMenu(BuildContext context, String path) async {
  HapticFeedback.mediumImpact();
  final video = isVideo(path);
  final raster = isPicture(path) && !isSvg(path);
  final name = fileName(path);
  Rect? origin;
  final box = context.findRenderObject();
  if (box is RenderBox && box.hasSize) {
    origin = box.localToGlobal(Offset.zero) & box.size;
  }
  final pick = await showCupertinoModalPopup<String>(
    context: context,
    builder: (ctx) => CupertinoActionSheet(
      title: Text(name),
      actions: [
        for (final a in [
          'Open',
          'Share',
          if (raster || video) 'Save to Photos',
          if (raster && canCopyImage) 'Copy image',
        ])
          CupertinoActionSheetAction(
            onPressed: () => Navigator.pop(ctx, a),
            child: Text(a),
          ),
      ],
      cancelButton: CupertinoActionSheetAction(
        onPressed: () => Navigator.pop(ctx),
        child: const Text('Cancel'),
      ),
    ),
  );
  if (pick == null || !context.mounted) return;
  if (pick == 'Open') return openAttached(context, path);
  final c = ChatScope.of(context);
  final Uint8List bytes;
  try {
    bytes = await c.readFile(path);
  } catch (e) {
    if (context.mounted) toast(context, "can't read $name: ${errorText(e)}");
    return;
  }
  if (!context.mounted) return;
  switch (pick) {
    case 'Share':
      await shareBytes(bytes, name, origin: origin);
    case 'Save to Photos':
      final why = await saveToPhotos(bytes, name, video: video);
      if (context.mounted) toast(context, why ?? 'saved to Photos');
    case 'Copy image':
      final why = await copyImage(bytes);
      if (context.mounted) toast(context, why ?? 'copied');
  }
}

/// The files of a message: pictures as thumbnails ([size] square, or [wide] boxes), videos as
/// ▶ tiles, anything else as a name chip.
class AttachedFiles extends StatelessWidget {
  const AttachedFiles({
    super.key,
    required this.paths,
    this.size = 96,
    this.wide = false,
    this.alignEnd = false,
  });
  final List<String> paths;
  final double size;
  final bool wide;
  final bool alignEnd;

  @override
  Widget build(BuildContext context) {
    if (paths.isEmpty) return const SizedBox.shrink();
    return Padding(
      padding: const EdgeInsets.only(top: 6),
      child: Wrap(
        spacing: 6,
        runSpacing: 6,
        alignment: alignEnd ? WrapAlignment.end : WrapAlignment.start,
        children: [
          for (final p in paths)
            if (isPicture(p))
              _Thumb(path: p, size: size, wide: wide)
            else if (isVideo(p))
              _VideoTile(path: p, size: size, wide: wide)
            else
              _FileChip(path: p),
        ],
      ),
    );
  }
}

/// SVGs that failed to parse, by `sid\npath`: drawn as file chips from then on.
final _badSvg = <String>{};

class _Thumb extends StatefulWidget {
  const _Thumb({required this.path, required this.size, required this.wide});
  final String path;
  final double size;
  final bool wide;

  @override
  State<_Thumb> createState() => _ThumbState();
}

class _ThumbState extends State<_Thumb> {
  String get path => widget.path;

  /// An SVG that does not parse turns into the file chip (after this frame).
  Widget _svgFailed(String key) {
    if (_badSvg.add(key)) {
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (mounted) setState(() {});
      });
    }
    return const SizedBox.shrink();
  }

  @override
  Widget build(BuildContext context) {
    final c = ChatScope.of(context);
    final key = '${c.sid}\n$path';
    if (_badSvg.contains(key)) return _FileChip(path: path);
    final bytes = c.picture(path);
    final err = c.pictureError(path);
    final wide = widget.wide;
    final w = wide ? double.infinity : widget.size;
    final h = wide ? 240.0 : widget.size;
    final fit = wide ? BoxFit.contain : BoxFit.cover;
    Widget child;
    if (bytes != null && isSvg(path)) {
      child = SvgPicture.memory(
        bytes,
        fit: fit,
        width: w,
        height: h,
        placeholderBuilder: (_) =>
            const Center(child: CupertinoActivityIndicator()),
        errorBuilder: (_, _, _) => _svgFailed(key),
      );
    } else if (bytes != null) {
      child = Image.memory(
        bytes,
        fit: fit,
        width: w,
        height: h,
        errorBuilder: (_, _, _) => _broken(context, fileName(path)),
      );
    } else if (err != null) {
      child = GestureDetector(
        onTap: () => c.retryPicture(path),
        child: _broken(context, '${fileName(path)}\n$err · retry'),
      );
    } else {
      child = const Center(child: CupertinoActivityIndicator());
    }
    return GestureDetector(
      onTap: bytes == null ? null : () => openAttached(context, path),
      onLongPress: bytes == null ? null : () => attachedMenu(context, path),
      child: Container(
        width: w,
        height: h,
        constraints: wide ? const BoxConstraints(maxWidth: 360) : null,
        clipBehavior: Clip.hardEdge,
        decoration: BoxDecoration(
          color: panel(context),
          border: Border.all(color: ink(context), width: 1.2),
        ),
        child: child,
      ),
    );
  }

  Widget _broken(BuildContext context, String text) => Center(
    child: Padding(
      padding: const EdgeInsets.all(4),
      child: Text(
        text,
        textAlign: TextAlign.center,
        maxLines: 4,
        overflow: TextOverflow.ellipsis,
        style: mono(context, size: 11, color: muted(context)),
      ),
    ),
  );
}

/// A video: its first frame with a ▶ (fetched like a picture, see [VideoPoster]), or a plain
/// ▶ tile with its name while it loads, where the platform has no player, or when it can't open.
class _VideoTile extends StatelessWidget {
  const _VideoTile({
    required this.path,
    required this.size,
    required this.wide,
  });
  final String path;
  final double size;
  final bool wide;

  @override
  Widget build(BuildContext context) {
    final plain = _plain(context);
    Widget child = plain;
    if (PosterCache.shared.supported) {
      final c = ChatScope.of(context);
      final bytes = c.picture(path);
      if (bytes != null) {
        child = VideoPoster(
          blobKey: '${c.sid}\n$path',
          bytes: bytes,
          name: fileName(path),
          fallback: plain,
        );
      }
    }
    return GestureDetector(
      onTap: () => openAttached(context, path),
      onLongPress: () => attachedMenu(context, path),
      child: Container(
        width: wide ? double.infinity : size,
        height: wide ? 200 : size,
        constraints: wide ? const BoxConstraints(maxWidth: 360) : null,
        clipBehavior: Clip.hardEdge,
        decoration: BoxDecoration(
          color: FemmeColors.ink,
          border: Border.all(color: ink(context), width: 1.2),
        ),
        child: child,
      ),
    );
  }

  Widget _plain(BuildContext context) => Column(
    mainAxisAlignment: MainAxisAlignment.center,
    children: [
      const Icon(
        CupertinoIcons.play_circle_fill,
        color: FemmeColors.paper,
        size: 36,
      ),
      Padding(
        padding: const EdgeInsets.all(4),
        child: Text(
          fileName(path),
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: mono(context, size: 11, color: FemmeColors.paper),
        ),
      ),
    ],
  );
}

class _FileChip extends StatelessWidget {
  const _FileChip({required this.path});
  final String path;

  @override
  Widget build(BuildContext context) => Chip(
    label: '📎 ${fileName(path)}',
    onTap: () => openAttached(context, path),
  );
}
