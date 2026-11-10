import 'package:flutter/cupertino.dart';

import '../state/prefs.dart';
import 'femme.dart';
import 'widgets.dart';

/// Placeholder Me tab: the paired box, the link, the theme, sign out.
class MeTab extends StatelessWidget {
  const MeTab({super.key});

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final link = f.link.state.value;
    return TabPage(
      title: 'Me',
      status: linkLine(link),
      slivers: [
        SliverToBoxAdapter(
          child: Column(
            children: [
              CupertinoListSection(
                header: const Text('Box'),
                children: [
                  CupertinoListTile(
                    title: Text(f.prefs.host ?? '—'),
                    subtitle: Text(linkLine(link)),
                  ),
                  CupertinoListTile(
                    title: const Text('Client instance'),
                    additionalInfo: Text('${f.prefs.clientInstance}'),
                  ),
                ],
              ),
              CupertinoListSection(
                header: const Text('Appearance'),
                children: [
                  Padding(
                    padding: const EdgeInsets.all(12),
                    child: CupertinoSlidingSegmentedControl<ThemePref>(
                      groupValue: f.prefs.theme,
                      onValueChanged: (t) {
                        if (t != null) f.prefs.setTheme(t);
                      },
                      children: const {
                        ThemePref.system: Text('System'),
                        ThemePref.light: Text('Light'),
                        ThemePref.dark: Text('Dark'),
                      },
                    ),
                  ),
                ],
              ),
              CupertinoListSection(
                children: [
                  CupertinoListTile(
                    title: const Text(
                      'Forget this box',
                      style: TextStyle(color: CupertinoColors.destructiveRed),
                    ),
                    onTap: () => f.prefs.forget(),
                  ),
                ],
              ),
            ],
          ),
        ),
      ],
    );
  }
}
