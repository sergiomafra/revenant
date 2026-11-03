import { act, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { EngineErrorView } from '../bridge';
import type { DiskView } from '../disks/types';
import i18n from '../i18n';
import { HD, SD, SSD, SYSTEM } from '../test-disks';
import { fakeBackend } from '../test-utils';
import ChooseDisk from './ChooseDisk';

/** Lets the pending commands answer and runs the timers due in `ms`. */
async function tick(ms = 0) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

/** Answers `list_disks` with each list in turn (the last one repeats); a string rejects with that code. */
function disks(...answers: (DiskView[] | string)[]) {
  let n = 0;
  return fakeBackend({
    list_disks: () => {
      const a = answers[Math.min(n++, answers.length - 1)];
      if (typeof a === 'string') throw { code: a };
      return a;
    },
  });
}

function open(props: { onContinue?: (d: DiskView) => void; onError?: (e: EngineErrorView) => void } = {}) {
  render(<ChooseDisk onContinue={props.onContinue ?? (() => {})} onError={props.onError ?? (() => {})} />);
}

const noRawKeys = () =>
  expect(document.body.textContent).not.toMatch(/\b(disks|disk|hint|steps|footer|permission|granted)\.[a-z_]+/);

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe('ChooseDisk', () => {
  it('shows_highlighted_disks_and_greys_the_system_disk', async () => {
    disks([SD, HD, SYSTEM]);
    open();
    await tick();
    expect(screen.getByRole('heading', { name: 'O que você quer recuperar?' })).toBeInTheDocument();
    const sd = screen.getByRole('radio', { name: /EOS_DIGITAL · 64 GB/ });
    expect(sd).toHaveTextContent('exFAT');
    expect(screen.getByRole('radio', { name: /LaCie Rugged · 2 TB/ })).toHaveTextContent('LaCie · ligado por USB');
    // The system disk is shown, greyed, with the reason, and cannot be chosen.
    expect(screen.queryByRole('radio', { name: /Macintosh HD/ })).not.toBeInTheDocument();
    const sys = screen.getByText('Macintosh HD').closest('.card');
    expect(sys).toHaveClass('off');
    expect(within(sys as HTMLElement).getByText('Disco do sistema: não dá para procurar nele')).toBeInTheDocument();
    // Nothing chosen yet: Continue waits; the hint talks about the card.
    expect(screen.getByRole('button', { name: 'Continuar' })).toBeDisabled();
    expect(screen.getByText('Pare de usar o cartão agora.')).toBeInTheDocument();
    expect(screen.getByText('Nada é alterado no cartão')).toBeInTheDocument();
    fireEvent.click(sd);
    expect(sd).toHaveAttribute('aria-checked', 'true');
    expect(within(sd).getByText('Escolhido')).toBeVisible();
    expect(screen.getByRole('button', { name: 'Continuar' })).toBeEnabled();
    noRawKeys();
  });

  for (const [lang, title, sub] of [
    ['es', '¿Qué quieres recuperar?', 'LaCie · conectado por USB'],
    ['en', 'What do you want to recover?', 'LaCie · connected by USB'],
  ] as const) {
    it(`shows_highlighted_disks_and_greys_the_system_disk (${lang})`, async () => {
      await i18n.changeLanguage(lang);
      disks([SD, HD, SSD, SYSTEM]);
      open();
      await tick();
      expect(screen.getByRole('heading', { name: title })).toBeInTheDocument();
      expect(screen.getByRole('radio', { name: /LaCie Rugged · 2 TB/ })).toHaveTextContent(sub);
      expect(screen.getByText('Macintosh HD').closest('.card')).toHaveClass('off');
      fireEvent.click(screen.getByRole('radio', { name: /T7 · 1 TB/ }));
      noRawKeys();
    });
  }

  it('empty_state_when_no_disks', async () => {
    disks([]);
    open();
    await tick();
    expect(screen.getByRole('heading', { name: 'Conecte o cartão ou o disco' })).toBeInTheDocument();
    expect(screen.getByText(/Ele aparece aqui sozinho/)).toBeInTheDocument();
    expect(screen.queryByRole('radio')).not.toBeInTheDocument();
    expect(screen.getByText('Pare de usar o cartão agora.')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Continuar' })).toBeDisabled();
  });

  it('ssd_choice_shows_the_caution', async () => {
    const onContinue = vi.fn();
    disks([SD, SSD, SYSTEM]);
    open({ onContinue });
    await tick();
    expect(screen.getByText(/SSD e cartões CFexpress/)).toBeInTheDocument();
    expect(screen.queryByText(/o sistema limpa sozinho/)).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('radio', { name: /T7 · 1 TB/ }));
    expect(screen.getByText(/o sistema limpa sozinho/)).toBeVisible();
    expect(screen.getByText('Pare de usar o disco agora.')).toBeInTheDocument();
    // "Learn why" folds and unfolds the explanation; the SSD can still be used.
    fireEvent.click(screen.getByRole('button', { name: 'Saiba por quê' }));
    expect(screen.queryByText(/o sistema limpa sozinho/)).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Saiba por quê' }));
    expect(screen.getByText(/o sistema limpa sozinho/)).toBeVisible();
    fireEvent.click(screen.getByRole('button', { name: 'Continuar' }));
    expect(onContinue).toHaveBeenCalledWith(SSD);
  });

  it('no caution when no disk that can be chosen is solid state', async () => {
    disks([SD, HD, SYSTEM]);
    open();
    await tick();
    expect(screen.queryByText(/SSD e cartões CFexpress/)).not.toBeInTheDocument();
  });

  it('camera_card_subtitle', async () => {
    disks([SD, { ...SD, id: 'disk7', camera_card: false }]);
    open();
    await tick();
    const [camera, plain] = screen.getAllByRole('radio');
    expect(camera).toHaveTextContent('SanDisk Extreme · parece um cartão de câmera');
    expect(plain).toHaveTextContent('SanDisk Extreme · num leitor de cartão');
    expect(plain).not.toHaveTextContent('câmera');
  });

  it('poll_updates_without_losing_selection', async () => {
    const calls = disks([HD, SYSTEM], [SD, HD, SYSTEM], [SD, SYSTEM]);
    open();
    await tick();
    expect(screen.getAllByRole('radio')).toHaveLength(1);
    const hd = screen.getByRole('radio', { name: /LaCie/ });
    fireEvent.click(hd);
    await tick(2000);
    expect(calls.filter((c) => c.cmd === 'list_disks')).toHaveLength(2);
    expect(screen.getAllByRole('radio')).toHaveLength(2);
    expect(screen.getByRole('radio', { name: /LaCie/ })).toHaveAttribute('aria-checked', 'true');
    expect(screen.getByRole('radio', { name: /EOS_DIGITAL/ })).toHaveAttribute('aria-checked', 'false');
    // The same list again does not redraw the cards.
    const before = screen.getByRole('radio', { name: /LaCie/ });
    await tick(0);
    expect(screen.getByRole('radio', { name: /LaCie/ })).toBe(before);
    // The chosen disk leaves: nothing is chosen any more.
    await tick(2000);
    expect(screen.queryByRole('radio', { name: /LaCie/ })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Continuar' })).toBeDisabled();
  });

  it('an unchanged list keeps the same cards', async () => {
    disks([SD, SYSTEM]);
    open();
    await tick();
    const before = screen.getByRole('radio', { name: /EOS_DIGITAL/ });
    await tick(2000);
    await tick(2000);
    expect(screen.getByRole('radio', { name: /EOS_DIGITAL/ })).toBe(before);
  });

  it('poll_failure_keeps_last_list', async () => {
    const onError = vi.fn();
    const calls = disks([SD, SYSTEM], 'system_tool_failed', [SD, HD, SYSTEM]);
    open({ onError });
    await tick();
    fireEvent.click(screen.getByRole('radio', { name: /EOS_DIGITAL/ }));
    await tick(2000);
    expect(onError).not.toHaveBeenCalled();
    expect(screen.getByRole('radio', { name: /EOS_DIGITAL/ })).toHaveAttribute('aria-checked', 'true');
    // It tries again on the next round.
    await tick(2000);
    expect(calls.filter((c) => c.cmd === 'list_disks')).toHaveLength(3);
    expect(screen.getAllByRole('radio')).toHaveLength(2);
  });

  it('first_load_failure_shows_error_screen (reports the code)', async () => {
    const onError = vi.fn();
    disks('system_tool_failed');
    open({ onError });
    await tick();
    expect(onError).toHaveBeenCalledWith({ code: 'system_tool_failed' });
  });

  it('shows the notice it is given (the chosen disk was not found)', async () => {
    disks([SD]);
    render(<ChooseDisk notice="disk_gone" onContinue={() => {}} onError={() => {}} />);
    await tick();
    expect(
      screen.getByText('O disco não foi encontrado. Confira se ele continua conectado.'),
    ).toBeInTheDocument();
  });

  it('keeps the disk chosen before when it is still there', async () => {
    disks([SD, HD]);
    render(<ChooseDisk initialSelected="disk5" onContinue={() => {}} onError={() => {}} />);
    await tick();
    expect(screen.getByRole('radio', { name: /LaCie/ })).toHaveAttribute('aria-checked', 'true');
  });
});
