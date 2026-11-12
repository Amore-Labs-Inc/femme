import 'package:flutter/cupertino.dart';

import '../state/prefs.dart';
import 'femme.dart';
import 'me/pairing.dart';
import 'theme.dart';

/// First run: pair the phone with a box (scan, paste or type).
class PairScreen extends StatelessWidget {
  const PairScreen({super.key, required this.prefs});
  final Prefs prefs;

  @override
  Widget build(BuildContext context) => CupertinoPageScaffold(
    backgroundColor: CupertinoColors.systemGroupedBackground,
    navigationBar: const CupertinoNavigationBar(
      middle: Text('Pair with your box'),
    ),
    child: SafeArea(
      child: ListView(
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(20, 24, 20, 0),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                const Text(
                  'FEMME',
                  style: TextStyle(
                    fontSize: 34,
                    fontWeight: FontWeight.w800,
                    color: FemmeColors.accent,
                  ),
                ),
                const SizedBox(height: 8),
                Text(
                  'The agents live on your box; this phone is where you glance, answer '
                  'and nudge them. Run `femmed pair` on your box, then scan the code it '
                  'prints.',
                  style: TextStyle(
                    fontSize: 16,
                    color: FemmeColors.mutedC.resolveFrom(context),
                  ),
                ),
              ],
            ),
          ),
          const PairForm(manual: true),
        ],
      ),
    ),
  );
}

/// Pairs again from inside the app (the box refused this phone's token): the pairing form on the
/// current navigator; saving it pops the page and reconnects at once, even when the host and
/// token did not change (the box may have enrolled the same token again).
Future<void> pushPairAgain(BuildContext context) {
  final link = Femme.of(context).link;
  return Navigator.of(context).push(
    CupertinoPageRoute<void>(
      builder: (c) => CupertinoPageScaffold(
        backgroundColor: CupertinoColors.systemGroupedBackground,
        navigationBar: const CupertinoNavigationBar(middle: Text('Pair again')),
        child: SafeArea(
          child: ListView(
            children: [
              Padding(
                padding: const EdgeInsets.fromLTRB(20, 16, 20, 0),
                child: Text(
                  'The box no longer knows this phone. Run `femmed pair` on your box and '
                  'scan the code it prints.',
                  style: TextStyle(
                    fontSize: 15,
                    color: FemmeColors.mutedC.resolveFrom(c),
                  ),
                ),
              ),
              PairForm(
                onPaired: () {
                  Navigator.of(c).pop();
                  link.reconnect();
                },
              ),
            ],
          ),
        ),
      ),
    ),
  );
}
