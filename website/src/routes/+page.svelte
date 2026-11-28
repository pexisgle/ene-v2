<script lang="ts">
  import {
    FLOW_STEPS,
    GUIDE_STEPS,
    NAV_ITEMS,
    SITE_META,
    TASK_EXAMPLES,
  } from "#lib/site-content.ts";
</script>

<svelte:head>
  <title>{SITE_META.title}</title>
  <meta name="description" content={SITE_META.description} />
  <meta property="og:title" content={SITE_META.ogTitle} />
  <meta property="og:description" content={SITE_META.ogDescription} />
  <meta property="og:type" content="website" />
</svelte:head>

<a class="skip" href="#main">本文へ移動</a>

<header class="site-header">
  <div class="wrap header-inner">
    <a class="site-name" href="#top" aria-label="ene トップ">ene</a>
    <nav aria-label="ページ内ナビゲーション">
      {#each NAV_ITEMS as item (item.href)}
        <a href={item.href}>{item.label}</a>
      {/each}
    </nav>
  </div>
</header>

<main id="main">
  <section class="hero wrap" id="top" aria-labelledby="intro">
    <div class="hero-copy">
      <h1 id="intro">パソコン作業を、<br /><em>会話で頼める。</em></h1>
      <p>
        ene は、画面のすみにいる AI
        コンパニオンです。ふだんの言葉で話しかけて、調べものやパソコン作業を頼めます。
      </p>
      <div class="hero-actions">
        <a class="button" href="#examples">機能一覧を見る</a>
        <a class="text-link" href="#guide">使い方を見る</a>
      </div>
    </div>
    <figure class="hero-view">
      <img
        src="/assets/desktop.webp"
        width="720"
        height="525"
        alt="Windowsデスクトップ上に表示されたeneの3Dアバターと会話ポッド"
        fetchpriority="high"
      />
      <figcaption>Windows版の実画面（展示用の会話例）</figcaption>
    </figure>
  </section>

  <section class="examples section" id="examples" aria-labelledby="examples-title">
    <div class="wrap">
      <div class="section-intro">
        <h2 id="examples-title">ene にできること</h2>
      </div>
      <ul class="task-list">
        {#each TASK_EXAMPLES as task (task.title)}
          <li class="task">
            {#if task.scope}
              <div>
                <h3>{task.title}</h3>
                <p class="task-scope">{task.scope}</p>
              </div>
            {:else}
              <h3>{task.title}</h3>
            {/if}
            <dl>
              <div class="task-request">
                <dt>{task.requestLabel}</dt>
                <dd>
                  {#each task.requestSegments as segment, index (index)}
                    {#if segment.code}
                      <code>{segment.text}</code>
                    {:else}
                      {segment.text}
                    {/if}
                  {/each}
                </dd>
              </div>
              <div>
                <dt>{task.responseLabel}</dt>
                <dd>
                  {task.responseText}
                  {#if task.responseLink}
                    <br />
                    <a href={task.responseLink.href}>{task.responseLink.label}</a>
                  {/if}
                </dd>
              </div>
            </dl>
          </li>
        {/each}
      </ul>
      <a class="text-link" href="#inside">メモをまとめる依頼を図でたどる</a>
    </div>
  </section>

  <section class="guide section" id="guide" aria-labelledby="guide-title">
    <div class="wrap">
      <div class="section-intro">
        <h2 id="guide-title">画面のすみから話しかける</h2>
      </div>
      <div class="walkthrough">
        <ol class="chat-steps">
          {#each GUIDE_STEPS as step (step.number)}
            <li>
              <span class="step-number" aria-hidden="true">{step.number}</span>
              <div>
                <h3>{step.title}</h3>
                <p>
                  {#each step.segments as segment, index (index)}
                    {#if segment.strong}
                      <strong>{segment.text}</strong>
                    {:else}
                      {segment.text}
                    {/if}
                  {/each}
                </p>
              </div>
            </li>
          {/each}
        </ol>
        <figure class="guide-figure">
          <a class="screen" href="/assets/chat.webp" aria-label="会話画面を拡大">
            <span class="chat-capture">
              <img
                src="/assets/chat.webp"
                width="460"
                height="549"
                alt="デスクトップのeneに展示の準備について話しかけた会話履歴と、下にあるメッセージ入力欄"
                loading="lazy"
              />
              <span class="step-number capture-input" aria-hidden="true">1</span>
              <span class="step-number capture-reply" aria-hidden="true">2</span>
            </span>
          </a>
          <figcaption>Windows版の会話画面（展示用の会話例）</figcaption>
        </figure>
      </div>
    </div>
  </section>

  <section class="inside section" id="inside" aria-labelledby="inside-title">
    <div class="wrap">
      <div class="section-intro">
        <h2 id="inside-title">頼んだあと、ene の中では？</h2>
        <p>
          会話を受け持つ<strong>「頭」</strong>が、作業を<strong>「手」</strong>に頼みます。<br
            class="wide"
          />「手」がファイルを読んでいる間も、「頭」と会話できます。
        </p>
      </div>
      <figure class="diagram" aria-labelledby="diagram-label">
        <figcaption id="diagram-label">お願いから返事まで</figcaption>
        <div class="work-map">
          <div class="node visitor">
            <h3>あなた</h3>
            <p>いつもの言葉で頼む</p>
          </div>
          <div class="horizontal-flows dialogue" aria-hidden="true">
            <span>依頼<i class="direction right"></i></span>
            <span><i class="direction left"></i>返事</span>
          </div>
          <div class="node brain">
            <h3>頭</h3>
            <p>会話する</p>
          </div>
          <div class="horizontal-flows assignment" aria-hidden="true">
            <span>指示<i class="direction right"></i></span>
            <span><i class="direction left"></i>報告</span>
          </div>
          <div class="node hands">
            <h3>手</h3>
            <p>作業する</p>
          </div>
        </div>
      </figure>
      <fieldset class="follow">
        <legend>メモを読んでもらう例でたどる</legend>
        {#each FLOW_STEPS as step (step.id)}
          <input
            type="radio"
            name="flow"
            value={step.id}
            id="flow-{step.id}"
            checked={step.id === "talk"}
          />
          <label for="flow-{step.id}">{step.tabLabel}</label>
        {/each}
        <div class="flow-copy" aria-live="polite" aria-atomic="true">
          {#each FLOW_STEPS as step (step.id)}
            <p class="flow-{step.id}">
              <strong>{step.heading}</strong>
              {step.description}
            </p>
          {/each}
        </div>
      </fieldset>
      <details class="role-details">
        <summary>会話を覚える仕組み</summary>
        <dl>
          <dt>心と記憶</dt>
          <dd>「頭」が ene らしい話し方や、以前のやり取りを会話に活かします。</dd>
        </dl>
        <p>
          <a class="text-link" href={SITE_META.architectureDocUrl}>開発者向けの構成資料</a>
        </p>
      </details>
    </div>
  </section>
</main>

<footer class="site-footer">
  <div class="wrap footer-inner">
    <a class="site-name" href="#top" aria-label="ene トップ">ene</a>
    <a class="text-link" href={SITE_META.githubUrl}>GitHub</a>
  </div>
</footer>
