import 'package:flutter/cupertino.dart';

import '../state/prefs.dart';
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
