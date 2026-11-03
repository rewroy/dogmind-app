# Security

Dogmind runs commands and edits files on your PC for a model, so a way around its sandbox or its approvals is a security
problem. For example: a command that writes outside the workspace or reaches the network without approval, a read of a
secret that should have asked first, or a web page or file whose words make the agent act without the person.

Please report such a problem privately, with **Report a vulnerability** on this repository's *Security* tab, and not in a
public issue. Say what you did, what happened and what you expected. A `/report` file helps, and it has no key or
conversation in it. Only the latest release gets fixes.

# セキュリティ

Dogmind はモデルのために、あなたの PC でコマンドを実行し、ファイルを編集します。そのため、サンドボックスや承認をすり抜けられることはセキュリティ上の問題です。たとえば次のようなものです。

- 承認なしにワークスペースの外へ書き込む、またはネットワークにつながるコマンド
- 確認を求めるべき秘密情報を、確認なしに読み取ること
- Web ページやファイルの文面によって、本人の指示なしにエージェントが動くこと

このような問題は公開の Issue ではなく、このリポジトリの *Security* タブにある **Report a vulnerability** から非公開で報告してください。何をして、何が起き、本来どうなるはずだったかを書いてください。`/report` で作るファイルがあると助かります。このファイルにはキーや会話は含まれません。修正するのは最新のリリースだけです。
