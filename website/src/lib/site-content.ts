export interface NavItem {
  href: string;
  label: string;
}

export interface RichSegment {
  text: string;
  code?: boolean;
  strong?: boolean;
}

export interface ExternalLink {
  href: string;
  label: string;
}

export interface TaskExample {
  title: string;
  scope?: string;
  requestLabel: string;
  requestSegments: RichSegment[];
  responseLabel: string;
  responseText: string;
  responseLink?: ExternalLink;
}

export interface GuideStep {
  number: number;
  title: string;
  segments: RichSegment[];
}

export type FlowStepId = "talk" | "delegate" | "work" | "reply";

export interface FlowStep {
  id: FlowStepId;
  tabLabel: string;
  heading: string;
  description: string;
}

export const SITE_META = {
  title: "ene — パソコン作業を、会話で頼める。",
  description:
    "パソコン作業を会話で頼める AI コンパニオン ene。アバターの動き、作業中の見守り、Web検索や画面操作などを例で紹介します。",
  ogTitle: "ene — パソコン作業を、会話で頼める。",
  ogDescription:
    "画面のすみにいる AI コンパニオン。具体的な作業例と、会話する「頭」や作業する「手」の仕組みを紹介します。",
  githubUrl: "https://github.com/pexisgle/ene-v2",
  architectureDocUrl: "https://github.com/pexisgle/ene-v2/blob/main/docs/architecture.md",
} as const;

export const NAV_ITEMS: readonly NavItem[] = [
  { href: "#examples", label: "機能一覧" },
  { href: "#guide", label: "使い方" },
  { href: "#inside", label: "仕組み" },
];

export const TASK_EXAMPLES: readonly TaskExample[] = [
  {
    title: "アバターが動く",
    requestLabel: "話しかけ方の例",
    requestSegments: [{ text: "「手を振って」" }],
    responseLabel: "ene の反応",
    responseText:
      "3Dアバターが手を振ります。会話に合わせてうなずいたり、待ち時間に伸びをしたりもします。",
  },
  {
    title: "作業中のエラーに気づく",
    scope: "Windows版で画面の見守りとWeb検索が使えるとき",
    requestLabel: "状況の例",
    requestSegments: [
      { text: "Windows の更新が止まり、画面にエラー " },
      { text: "0x80070070", code: true },
      { text: " が出る。" },
    ],
    responseLabel: "ene の助け方の例",
    responseText:
      "ene が裏で原因を調べ、対処のヒントを知らせることがあります。たとえば「空き容量が足りないときのエラーだよ。まずストレージを確認してみて」。",
    responseLink: {
      href: "https://learn.microsoft.com/ja-jp/troubleshoot/windows-server/installing-updates-features-roles/troubleshoot-windows-update-error-0x80070070",
      label: "エラーの出典（Microsoft）",
    },
  },
  {
    title: "Webで調べものをする",
    scope: "Web検索を使えるとき",
    requestLabel: "あなたの頼み方",
    requestSegments: [{ text: "「『走れメロス』が読めるページを探して」" }],
    responseLabel: "ene の返事の例",
    responseText: "本文を読めるリンクを返します。",
    responseLink: {
      href: "https://www.aozora.gr.jp/cards/000035/card1567.html",
      label: "青空文庫『走れメロス』",
    },
  },
  {
    title: "開いているアプリを操作する",
    scope: "Windows版で画面操作を使えるとき",
    requestLabel: "あなたの頼み方",
    requestSegments: [{ text: "「開いているメモ帳に『次のデモは10時半です。』と入力して」" }],
    responseLabel: "操作の結果",
    responseText: "メモ帳の本文欄に「次のデモは10時半です。」と入力されます。",
  },
  {
    title: "ファイルを使って作業する",
    scope: "読み取り・検索・書き出し",
    requestLabel: "あなたの頼み方",
    requestSegments: [{ text: "「展示メモ.txt を読んで、準備することを3つにまとめて」" }],
    responseLabel: "ene の返事の例",
    responseText: "「準備は、デモPCの起動、配布資料の用意、動作確認の3つです」",
  },
];

export const GUIDE_STEPS: readonly GuideStep[] = [
  {
    number: 1,
    title: "入力欄から送る",
    segments: [
      { text: "画面下の「メッセージを入力…」に書いて、" },
      { text: "Enter", strong: true },
      { text: " キーか送信ボタンを押します。" },
    ],
  },
  {
    number: 2,
    title: "吹き出しで返事を読む",
    segments: [
      { text: "ene の返事が上の吹き出しに届きます。丸いメニューの" },
      { text: "「履歴を開く」", strong: true },
      { text: "で、前のやり取りも読めます。" },
    ],
  },
];

export const FLOW_STEPS: readonly FlowStep[] = [
  {
    id: "talk",
    tabLabel: "1. 頼む",
    heading: "あなたが頼む",
    description: "「展示メモ.txt を読んで、準備することを3つにまとめて」と送ります。",
  },
  {
    id: "delegate",
    tabLabel: "2. 渡す",
    heading: "頭が手に依頼を渡す",
    description: "「展示メモ.txt を開いて、準備することを拾って」と「手」に伝えます。",
  },
  {
    id: "work",
    tabLabel: "3. 読む",
    heading: "手がメモを読む",
    description: "ファイルを開き、見つけた準備項目を「頭」に報告します。",
  },
  {
    id: "reply",
    tabLabel: "4. 返す",
    heading: "頭が会話に返す",
    description: "「デモPCを起動する・配布資料を用意する・動作を確認する」と、結果を伝えます。",
  },
];
