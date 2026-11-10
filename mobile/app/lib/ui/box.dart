import 'package:flutter/cupertino.dart';

import 'box/accounts.dart';
import 'box/info.dart';
import 'box/issues.dart';
import 'box/keys.dart';
import 'box/prs.dart';
import 'femme.dart';
import 'widgets.dart';

/// The machine: what femmed runs, the PRs waiting, the Claude accounts, the API keys, the issues.
class BoxTab extends StatelessWidget {
  const BoxTab({super.key});

  @override
  Widget build(BuildContext context) {
    final link = Femme.of(context).link.state.value;
    return TabPage(
      title: 'Box',
      status: linkLine(link),
      dot: linkColor(link),
      slivers: const [
        SliverToBoxAdapter(child: BoxInfoSection()),
        SliverToBoxAdapter(child: PrsSection()),
        SliverToBoxAdapter(child: AccountsSection()),
        SliverToBoxAdapter(child: KeysSection()),
        SliverToBoxAdapter(child: IssuesSection()),
      ],
    );
  }
}
