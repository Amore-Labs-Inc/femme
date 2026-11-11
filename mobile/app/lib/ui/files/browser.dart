import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../../net/rpc.dart';
import '../../state/files.dart';
import '../theme.dart';
import 'share.dart';
import 'viewer.dart';

/// The session folder, one folder at a time: a breadcrumb, the folder's rows, pull to refresh.
/// Each folder is its own page, so back (the bar's `‹`, the iOS edge swipe, Android's back)
/// climbs one folder, and from the session folder leaves the screen; a crumb pops back to its
/// folder.
class FilesBrowser extends StatefulWidget {
  const FilesBrowser({
    super.key,
    required this.tree,
    required this.cwd,
    required this.rootName,
  }) : dir = '';

  const FilesBrowser._folder({
    required this.tree,
    required this.cwd,
    required this.rootName,
    required this.dir,
  });

  final FilesTree tree;

  /// The session's folder on the box (for "Copy path").
  final String cwd;

  /// The first crumb: the session folder's name.
  final String rootName;

  /// The folder this page shows, relative to the session folder (`''` is the session folder).
  final String dir;

  @override
  State<FilesBrowser> createState() => _FilesBrowserState();
}

int _depth(String dir) => dir.isEmpty ? 0 : dir.split('/').length;

class _FilesBrowserState extends State<FilesBrowser> {
  String get _dir => widget.dir;

  @override
  void initState() {
    super.initState();
    widget.tree.ensure(_dir);
  }

  /// A crumb pops back to its folder's page (one page per level below the session folder); a
  /// sub-folder gets a page of its own.
  void _go(String dir) {
    if (dir == _dir) return;
    HapticFeedback.selectionClick();
    final up = _depth(_dir) - _depth(dir);
    if (up > 0) {
      var left = up;
      Navigator.of(context).popUntil((_) => left-- == 0);
      return;
    }
    // Listed before the page builds: a load notifies the tree, which the page below listens to.
    widget.tree.ensure(dir);
    Navigator.of(context).push(
      CupertinoPageRoute<void>(
        title: dir.split('/').last,
        builder: (_) => FilesBrowser._folder(
          tree: widget.tree,
          cwd: widget.cwd,
          rootName: widget.rootName,
          dir: dir,
        ),
      ),
    );
  }

  void _open(FsEntry e) {
    final path = joinPath(_dir, e.name);
    if (path == null) return;
    if (e.dir) return _go(path);
    HapticFeedback.selectionClick();
    Navigator.of(context).push(
      CupertinoPageRoute<void>(
        builder: (_) =>
            FileViewerPage(tree: widget.tree, path: path, len: e.len),
      ),
    );
  }

  void _actions(BuildContext rowContext, FsEntry e) {
    final path = joinPath(_dir, e.name);
    if (path == null) return;
    HapticFeedback.selectionClick();
    showCupertinoModalPopup<void>(
      context: context,
      builder: (c) => CupertinoActionSheet(
        title: Text(e.name),
        message: Text(e.dir ? 'Folder' : humanSize(e.len)),
        actions: [
          CupertinoActionSheetAction(
            onPressed: () {
              Navigator.pop(c);
              _open(e);
            },
            child: const Text('Open'),
          ),
          if (!e.dir)
            CupertinoActionSheetAction(
              onPressed: () {
                Navigator.pop(c);
                shareFile(rowContext, widget.tree, path, e.len);
              },
              child: const Text('Share'),
            ),
          CupertinoActionSheetAction(
            onPressed: () {
              Navigator.pop(c);
              copyPath(widget.cwd, path);
            },
            child: const Text('Copy path'),
          ),
        ],
        cancelButton: CupertinoActionSheetAction(
          isDefaultAction: true,
          onPressed: () => Navigator.pop(c),
          child: const Text('Cancel'),
        ),
      ),
    );
  }

  Future<void> _refresh() async {
    HapticFeedback.selectionClick();
    await widget.tree.refresh();
  }

  @override
  Widget build(BuildContext context) => CupertinoPageScaffold(
    child: ListenableBuilder(
      listenable: widget.tree,
      builder: (context, _) => CustomScrollView(
        physics: const AlwaysScrollableScrollPhysics(
          parent: BouncingScrollPhysics(),
        ),
        slivers: [
          CupertinoSliverNavigationBar(
            largeTitle: Text(_dir.isEmpty ? 'Files' : _dir.split('/').last),
          ),
          CupertinoSliverRefreshControl(onRefresh: _refresh),
          SliverToBoxAdapter(
            child: Crumbs(rootName: widget.rootName, dir: _dir, onTap: _go),
          ),
          ..._rows(context),
        ],
      ),
    ),
  );

  List<Widget> _rows(BuildContext context) {
    final d = widget.tree.dir(_dir);
    final entries = d?.entries;
    final muted = FemmeColors.mutedC.resolveFrom(context);
    Widget fill(Widget child) => SliverFillRemaining(
      hasScrollBody: false,
      child: Center(
        child: Padding(padding: const EdgeInsets.all(24), child: child),
      ),
    );
    if (d?.error != null && entries == null) {
      return [
        fill(
          Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(
                'Could not list this folder.\n${d!.error}',
                textAlign: TextAlign.center,
                style: TextStyle(color: muted),
              ),
              CupertinoButton(
                onPressed: () => widget.tree.load(_dir),
                child: const Text('Try again'),
              ),
            ],
          ),
        ),
      ];
    }
    if (entries == null) {
      return [fill(const CupertinoActivityIndicator())];
    }
    if (entries.isEmpty) {
      return [fill(Text('Empty folder', style: TextStyle(color: muted)))];
    }
    return [
      if (d?.error != null)
        SliverToBoxAdapter(
          child: Padding(
            padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
            child: Text(
              'Refresh failed: ${d!.error}',
              style: const TextStyle(color: FemmeColors.bad, fontSize: 13),
            ),
          ),
        ),
      SliverSafeArea(
        top: false,
        sliver: SliverList.builder(
          itemCount: entries.length,
          itemBuilder: (context, i) => FileRow(
            entry: entries[i],
            onTap: () => _open(entries[i]),
            onMore: (rowContext) => _actions(rowContext, entries[i]),
          ),
        ),
      ),
    ];
  }
}

/// The session folder's name, then each folder down to the one shown; a crumb makes its folder
/// the one shown.
class Crumbs extends StatelessWidget {
  const Crumbs({
    super.key,
    required this.rootName,
    required this.dir,
    required this.onTap,
  });

  final String rootName;
  final String dir;
  final ValueChanged<String> onTap;

  @override
  Widget build(BuildContext context) {
    final segs = dir.isEmpty ? const <String>[] : dir.split('/');
    final crumbs = <(String, String)>[('', rootName)];
    for (var i = 0; i < segs.length; i++) {
      crumbs.add((segs.sublist(0, i + 1).join('/'), segs[i]));
    }
    final ink = FemmeColors.inkC.resolveFrom(context);
    final muted = FemmeColors.mutedC.resolveFrom(context);
    return Container(
      height: 40,
      decoration: BoxDecoration(
        border: Border(
          bottom: BorderSide(
            color: CupertinoColors.separator.resolveFrom(context),
            width: 0,
          ),
        ),
      ),
      child: ListView.separated(
        scrollDirection: Axis.horizontal,
        reverse: true, // the deepest crumb stays in sight
        padding: const EdgeInsets.symmetric(horizontal: 12),
        itemCount: crumbs.length,
        separatorBuilder: (_, _) =>
            Icon(CupertinoIcons.chevron_right, size: 12, color: muted),
        itemBuilder: (context, j) {
          final i = crumbs.length - 1 - j;
          final (path, label) = crumbs[i];
          final last = i == crumbs.length - 1;
          return CupertinoButton(
            padding: const EdgeInsets.symmetric(horizontal: 6),
            minimumSize: const Size(44, 40),
            onPressed: last ? null : () => onTap(path),
            child: Text(
              label,
              style: TextStyle(
                fontSize: 15,
                color: last ? ink : FemmeColors.accent,
                fontWeight: last ? FontWeight.w600 : FontWeight.w400,
              ),
            ),
          );
        },
      ),
    );
  }
}

/// Where a [FileRow]'s name starts: [CupertinoListTile]'s 20 pt start padding, its 28 pt leading
/// and the 16 pt gap to the title. The row separator starts here.
const double fileRowTextInset = 20 + 28 + 16;

/// One entry: icon, name (dot-files muted), size or chevron, and a `…` for the actions.
class FileRow extends StatelessWidget {
  const FileRow({
    super.key,
    required this.entry,
    required this.onTap,
    required this.onMore,
  });

  final FsEntry entry;
  final VoidCallback onTap;
  final void Function(BuildContext rowContext) onMore;

  @override
  Widget build(BuildContext context) {
    final ink = FemmeColors.inkC.resolveFrom(context);
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final hidden = isHidden(entry.name);
    return GestureDetector(
      onLongPress: () => onMore(context),
      child: Stack(
        children: [
          CupertinoListTile(
            onTap: onTap,
            leading: Icon(
              entry.dir ? CupertinoIcons.folder : CupertinoIcons.doc,
              color: hidden ? muted : ink,
            ),
            title: Text(
              entry.name,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(color: hidden ? muted : ink),
            ),
            additionalInfo: entry.dir
                ? null
                : Text(
                    humanSize(entry.len),
                    style: TextStyle(color: muted, fontSize: 13),
                  ),
            trailing: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                CupertinoButton(
                  padding: EdgeInsets.zero,
                  minimumSize: const Size(36, 44),
                  onPressed: () => onMore(context),
                  child: Icon(
                    CupertinoIcons.ellipsis,
                    size: 20,
                    color: muted,
                    semanticLabel: 'More actions',
                  ),
                ),
                SizedBox(
                  width: 14,
                  child: entry.dir
                      ? Icon(
                          CupertinoIcons.chevron_forward,
                          size: 14,
                          color: muted,
                        )
                      : null,
                ),
              ],
            ),
          ),
          // The separator starts at the name, as in an inset grouped list.
          PositionedDirectional(
            start: fileRowTextInset,
            end: 0,
            bottom: 0,
            child: SizedBox(
              key: const ValueKey('file-row-separator'),
              height: 1 / MediaQuery.devicePixelRatioOf(context),
              child: ColoredBox(
                color: CupertinoColors.separator.resolveFrom(context),
              ),
            ),
          ),
        ],
      ),
    );
  }
}
