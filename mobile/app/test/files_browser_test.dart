import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/files.dart';
import 'package:femme_mobile/ui/files/browser.dart';
import 'package:femme_mobile/ui/files/text_view.dart';
import 'package:femme_mobile/ui/files/hex.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';

import 'files_fake.dart';

FakeBox box() => FakeBox(
  dirs: {
    '': const [
      FsEntry('README.md', false, 12),
      FsEntry('notes.txt', false, 11),
      FsEntry('src', true, 0),
      FsEntry('.env', false, 4),
      FsEntry('huge.bin', false, 20 << 20),
      FsEntry('blob', false, 3),
      FsEntry('mid.txt', false, 3 << 20),
    ],
    'src': const [FsEntry('ui', true, 0), FsEntry('main.rs', false, 2048)],
    'src/ui': const [],
  },
  files: {
    'README.md': 'hello\nworld\n'.codeUnits,
    'notes.txt': 'hello\nworld'.codeUnits,
    'blob': [0, 1, 2],
  },
);

Widget app(FilesTree tree) => CupertinoApp(
  home: FilesBrowser(tree: tree, cwd: '/home/u/proj', rootName: 'proj'),
);

void main() {
  testWidgets('lists the root, opens folders, crumbs climb back', (t) async {
    final b = box();
    final tree = FilesTree(requester: b, sid: 's1');
    await t.pumpWidget(app(tree));
    await t.pumpAndSettle();

    expect(find.text('Files'), findsWidgets);
    expect(find.text('proj'), findsOneWidget);
    expect(find.text('src'), findsOneWidget);
    expect(find.text('12 B'), findsOneWidget);
    expect(find.text('20.0 MiB'), findsOneWidget);
    expect(b.listed, ['']);

    await t.tap(find.text('src'));
    await t.pumpAndSettle();
    expect(b.listed, ['', 'src']);
    expect(find.text('main.rs'), findsOneWidget);
    expect(find.text('2.0 KiB'), findsOneWidget);

    await t.tap(find.text('ui'));
    await t.pumpAndSettle();
    expect(b.listed, ['', 'src', 'src/ui']);
    expect(find.text('Empty folder'), findsOneWidget);

    await t.tap(find.text('proj'));
    await t.pumpAndSettle();
    expect(find.text('README.md'), findsOneWidget);
    expect(b.listed, ['', 'src', 'src/ui'], reason: 'root already listed');
  });

  testWidgets('a text file opens in the lines view', (t) async {
    final b = box();
    final tree = FilesTree(requester: b, sid: 's1');
    await t.pumpWidget(app(tree));
    await t.pumpAndSettle();
    await t.tap(find.text('notes.txt'));
    await t.pumpAndSettle();
    expect(b.read, ['notes.txt']);
    expect(find.byType(LinesView), findsOneWidget);
    expect(find.text('hello'), findsOneWidget);
    expect(find.text('world'), findsOneWidget);
  });

  testWidgets('a binary file opens as hex', (t) async {
    final tree = FilesTree(requester: box(), sid: 's1');
    await t.pumpWidget(app(tree));
    await t.pumpAndSettle();
    await t.tap(find.text('blob'));
    await t.pumpAndSettle();
    expect(find.byType(HexView), findsOneWidget);
    expect(find.textContaining('00000000  00 01 02'), findsOneWidget);
  });

  testWidgets('a file over the read limit is refused, not read', (t) async {
    final b = box();
    final tree = FilesTree(requester: b, sid: 's1');
    await t.pumpWidget(app(tree));
    await t.pumpAndSettle();
    await t.tap(find.text('huge.bin'));
    await t.pumpAndSettle();
    expect(find.textContaining('the box sends at most'), findsOneWidget);
    expect(b.read, isEmpty);
  });

  testWidgets('the … button opens the actions', (t) async {
    final tree = FilesTree(requester: box(), sid: 's1');
    await t.pumpWidget(app(tree));
    await t.pumpAndSettle();
    Finder more(String name) => find.descendant(
      of: find.ancestor(of: find.text(name), matching: find.byType(FileRow)),
      matching: find.byIcon(CupertinoIcons.ellipsis),
    );
    await t.tap(more('src'));
    await t.pumpAndSettle();
    expect(find.text('Share'), findsNothing, reason: 'folders are not shared');
    expect(find.text('Copy path'), findsOneWidget);
    await t.tap(find.text('Cancel'));
    await t.pumpAndSettle();
    await t.tap(more('README.md'));
    await t.pumpAndSettle();
    expect(find.text('Share'), findsOneWidget);
    expect(find.text('Copy path'), findsOneWidget);
    await t.tap(find.text('Cancel'));
    await t.pumpAndSettle();
  });

  testWidgets('on Wi-Fi a 3 MiB file opens without asking', (t) async {
    final b = box()..files = {...box().files, 'mid.txt': 'ok'.codeUnits};
    final tree = FilesTree(requester: b, sid: 's1');
    await t.pumpWidget(app(tree));
    await t.pumpAndSettle();
    await t.tap(find.text('mid.txt'));
    await t.pumpAndSettle();
    expect(find.text('This file is 3.0 MiB.'), findsNothing);
    expect(b.read, ['mid.txt']);
    expect(find.text('ok'), findsOneWidget);
  });

  testWidgets('on cellular a file over 2 MiB asks first', (t) async {
    netProbe = () async => NetKind.cellular;
    addTearDown(() => netProbe = () => probeNet());
    final b = box()..files = {...box().files, 'mid.txt': 'ok'.codeUnits};
    final tree = FilesTree(requester: b, sid: 's1');
    await t.pumpWidget(app(tree));
    await t.pumpAndSettle();
    await t.tap(find.text('mid.txt'));
    await t.pumpAndSettle();
    expect(find.text('This file is 3.0 MiB.'), findsWidgets);
    expect(b.read, isEmpty);
    await t.tap(find.text('Open').last);
    await t.pumpAndSettle();
    expect(b.read, ['mid.txt']);
    expect(find.text('ok'), findsOneWidget);
  });

  group('back', () {
    // The Files screen pushed over a page, so leaving it shows `under`.
    Future<FakeBox> open(WidgetTester t) async {
      final b = box();
      final tree = FilesTree(requester: b, sid: 's1');
      await t.pumpWidget(
        CupertinoApp(
          home: Builder(
            builder: (context) => CupertinoPageScaffold(
              child: Center(
                child: CupertinoButton(
                  onPressed: () => Navigator.of(context).push(
                    CupertinoPageRoute<void>(
                      title: 'Chat',
                      builder: (_) => FilesBrowser(
                        tree: tree,
                        cwd: '/home/u/proj',
                        rootName: 'proj',
                      ),
                    ),
                  ),
                  child: const Text('under'),
                ),
              ),
            ),
          ),
        ),
      );
      await t.tap(find.text('under'));
      await t.pumpAndSettle();
      await t.tap(find.text('src'));
      await t.pumpAndSettle();
      await t.tap(find.text('ui'));
      await t.pumpAndSettle();
      expect(find.text('Empty folder'), findsOneWidget);
      return b;
    }

    testWidgets('system back climbs a folder, then leaves at the root', (
      t,
    ) async {
      await open(t);
      await t.binding.handlePopRoute();
      await t.pumpAndSettle();
      expect(find.text('main.rs'), findsOneWidget);
      await t.binding.handlePopRoute();
      await t.pumpAndSettle();
      expect(find.text('README.md'), findsOneWidget);
      await t.binding.handlePopRoute();
      await t.pumpAndSettle();
      expect(find.text('under'), findsOneWidget);
      expect(find.byType(FilesBrowser), findsNothing);
    });

    testWidgets('the bar\'s back chevron climbs a folder', (t) async {
      await open(t);
      await t.tap(find.byType(CupertinoNavigationBarBackButton));
      await t.pumpAndSettle();
      expect(find.text('main.rs'), findsOneWidget);
      await t.tap(find.byType(CupertinoNavigationBarBackButton));
      await t.pumpAndSettle();
      expect(find.text('README.md'), findsOneWidget);
      await t.tap(find.byType(CupertinoNavigationBarBackButton));
      await t.pumpAndSettle();
      expect(find.text('under'), findsOneWidget);
    });

    testWidgets('the edge swipe climbs a folder and leaves from the root', (
      t,
    ) async {
      await open(t);
      Future<void> swipe() async {
        await t.dragFrom(const Offset(4, 300), const Offset(600, 0));
        await t.pumpAndSettle();
      }

      await swipe();
      expect(find.text('main.rs'), findsOneWidget);
      await swipe();
      expect(find.text('README.md'), findsOneWidget);
      await swipe();
      expect(find.text('under'), findsOneWidget);
    }, variant: TargetPlatformVariant.only(TargetPlatform.iOS));

    testWidgets('a crumb pops back to its folder', (t) async {
      final b = await open(t);
      await t.tap(find.text('proj'));
      await t.pumpAndSettle();
      expect(find.text('README.md'), findsOneWidget);
      expect(b.listed, ['', 'src', 'src/ui']);
      await t.binding.handlePopRoute();
      await t.pumpAndSettle();
      expect(find.text('under'), findsOneWidget);
    });
  });

  testWidgets('no refresh button (pull to refresh does it); separators '
      'start at the name', (t) async {
    final tree = FilesTree(requester: box(), sid: 's1');
    await t.pumpWidget(app(tree));
    await t.pumpAndSettle();
    expect(find.byIcon(CupertinoIcons.arrow_clockwise), findsNothing);
    expect(find.bySemanticsLabel('Refresh'), findsNothing);
    expect(
      find.byType(CupertinoSliverRefreshControl, skipOffstage: false),
      findsOneWidget,
    );

    final row = find.ancestor(
      of: find.text('notes.txt'),
      matching: find.byType(FileRow),
    );
    final sep = find.descendant(
      of: row,
      matching: find.byKey(const ValueKey('file-row-separator')),
    );
    final name = t.getTopLeft(find.text('notes.txt')).dx;
    expect(t.getTopLeft(sep).dx - t.getTopLeft(row).dx, fileRowTextInset);
    expect(t.getTopLeft(sep).dx, name);
    expect(t.getTopRight(sep).dx, t.getTopRight(row).dx);
  });
}
