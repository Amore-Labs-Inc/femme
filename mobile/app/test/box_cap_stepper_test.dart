import 'package:femme_mobile/ui/box/accounts.dart';
import 'package:femme_mobile/state/box.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

/// A stepper whose value follows its own changes, as the Accounts section does.
class _Host extends StatefulWidget {
  const _Host(this.changes);
  final List<int?> changes;
  @override
  State<_Host> createState() => _HostState();
}

class _HostState extends State<_Host> {
  int? v = 80;
  @override
  Widget build(BuildContext context) => CupertinoApp(
    home: Center(
      child: SizedBox(
        width: 260,
        child: CapStepper(
          label: '5 h cap',
          value: v,
          onChanged: (n) {
            widget.changes.add(n);
            setState(() => v = n);
          },
        ),
      ),
    ),
  );
}

void main() {
  testWidgets(
    'holding + repeats every 80 ms after 400 ms, with a click each step',
    (t) async {
      final clicks = <MethodCall>[];
      t.binding.defaultBinaryMessenger.setMockMethodCallHandler(
        SystemChannels.platform,
        (c) async {
          if (c.method == 'HapticFeedback.vibrate') clicks.add(c);
          return null;
        },
      );
      final changes = <int?>[];
      await t.pumpWidget(_Host(changes));
      final g = await t.startGesture(
        t.getCenter(find.byIcon(CupertinoIcons.plus_circle)),
      );
      await t.pump();
      expect(changes, [81], reason: 'the press itself steps');
      await t.pump(const Duration(milliseconds: 390));
      expect(changes, [81], reason: 'no repeat before 400 ms');
      await t.pump(const Duration(milliseconds: 10 + 80));
      expect(changes, [81, 82]);
      for (var i = 0; i < 15; i++) {
        await t.pump(const Duration(milliseconds: 80));
      }
      expect(changes.last, 97);
      await g.up();
      final n = changes.length;
      await t.pump(const Duration(seconds: 1));
      expect(changes.length, n, reason: 'stops on release');
      expect(clicks.length, changes.length);
    },
  );

  testWidgets('a hold stops at the top of the range', (t) async {
    final changes = <int?>[];
    await t.pumpWidget(_Host(changes));
    final g = await t.startGesture(
      t.getCenter(find.byIcon(CupertinoIcons.plus_circle)),
    );
    await t.pump(const Duration(milliseconds: 100));
    for (var i = 0; i < 40; i++) {
      await t.pump(const Duration(milliseconds: 80));
    }
    await g.up();
    expect(changes.last, capMax);
    expect(changes.where((c) => c == capMax), hasLength(1));
  });

  testWidgets('every control is 44 x 44 and has a spoken name', (t) async {
    final h = t.ensureSemantics();
    await t.pumpWidget(_Host([]));
    for (final l in ['raise 5 h cap', 'lower 5 h cap', 'clear 5 h cap']) {
      final f = find.bySemanticsLabel(l);
      expect(f, findsOneWidget, reason: l);
      final s = t.getSize(f);
      expect(s.width, greaterThanOrEqualTo(44), reason: l);
      expect(s.height, greaterThanOrEqualTo(44), reason: l);
    }
    h.dispose();
  });

  testWidgets('usage tracks are plain rounded bars, no outline', (t) async {
    await t.pumpWidget(
      const CupertinoApp(
        home: Center(
          child: SizedBox(
            width: 200,
            child: UsageTrack(fill: .5, color: Color(0xFF03AA5C)),
          ),
        ),
      ),
    );
    final clip = t.widget<ClipRRect>(find.byType(ClipRRect));
    expect(clip.borderRadius, const BorderRadius.all(Radius.circular(4)));
    final boxes = t.widgetList<Container>(find.byType(Container));
    for (final b in boxes) {
      final d = b.decoration;
      expect(d is BoxDecoration ? d.border : null, isNull);
    }
    expect(t.getSize(find.byType(DecoratedBox).last).width, closeTo(100, .01));
  });
}
