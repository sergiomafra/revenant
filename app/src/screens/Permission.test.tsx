import { act, fireEvent, render, screen } from '@testing-library/react';
import type { ComponentProps } from 'react';
import { describe, expect, it, vi } from 'vitest';
import { SD } from '../test-disks';
import { fakeBackend } from '../test-utils';
import Permission, { platformOf, type Platform } from './Permission';

type Handlers = Partial<Pick<ComponentProps<typeof Permission>, 'onGranted' | 'onBack' | 'onGone' | 'onError'>>;
const none = () => {};
function show(os: Platform, h: Handlers = {}) {
  render(
    <Permission
      disk={SD}
      os={os}
      onGranted={h.onGranted ?? none}
      onBack={h.onBack ?? none}
      onGone={h.onGone ?? none}
      onError={h.onError ?? none}
    />,
  );
}

describe('Permission', () => {
  it('tells the Mac from Windows', () => {
    const mac = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)';
    const win = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0';
    expect(platformOf(mac, 'MacIntel')).toBe('mac');
    expect(platformOf(win, 'Win32')).toBe('windows');
    expect(platformOf(win, '')).toBe('windows');
    expect(platformOf('', '')).toBe('mac');
  });

  it('Mac: explains the administrator password', () => {
    fakeBackend({});
    show('mac');
    expect(screen.getByRole('heading', { name: 'Só falta uma permissão do Mac' })).toBeInTheDocument();
    expect(screen.getByText('O Revenant só lê. Ele nunca escreve no cartão.')).toBeInTheDocument();
    expect(screen.getByText('Como funciona')).toBeInTheDocument();
    expect(screen.getByText('Clique no botão')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Pedir a senha de administrador' })).toBeEnabled();
    expect(screen.getByText('Seus dados não saem do computador')).toBeInTheDocument();
  });

  it('Windows: explains the system question', () => {
    fakeBackend({});
    show('windows');
    expect(screen.getByRole('heading', { name: 'O Windows vai pedir a sua confirmação' })).toBeInTheDocument();
    expect(screen.getByText('O Windows pergunta se pode permitir. Clique em Sim.')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Continuar' })).toBeEnabled();
  });

  it('the button is busy while the system asks', async () => {
    let answer: (v: unknown) => void = () => {};
    fakeBackend({ request_access: () => new Promise((r) => (answer = r)) });
    const onGranted = vi.fn();
    show('mac', { onGranted });
    const ask = screen.getByRole('button', { name: 'Pedir a senha de administrador' });
    fireEvent.click(ask);
    expect(ask).toBeDisabled();
    expect(ask).toHaveAttribute('aria-busy', 'true');
    // The rest of the window still answers.
    expect(screen.getByRole('button', { name: 'Voltar' })).toBeEnabled();
    answer({ id: 'disk4', size: 64_000_000_000 });
    await vi.waitFor(() => expect(onGranted).toHaveBeenCalledWith({ id: 'disk4', size: 64_000_000_000 }));
  });

  it('two quick clicks ask the system once', async () => {
    let answer: (v: unknown) => void = () => {};
    const calls = fakeBackend({ request_access: () => new Promise((r) => (answer = r)) });
    const onGranted = vi.fn();
    show('mac', { onGranted });
    const ask = screen.getByRole('button', { name: 'Pedir a senha de administrador' });
    act(() => {
      ask.click();
      ask.click();
    });
    await vi.waitFor(() => expect(calls.filter((c) => c.cmd === 'request_access')).toHaveLength(1));
    answer({ id: 'disk4', size: 1 });
    await vi.waitFor(() => expect(onGranted).toHaveBeenCalledTimes(1));
    expect(calls.filter((c) => c.cmd === 'request_access')).toHaveLength(1);
  });

  it('another error code goes to the error screen', async () => {
    fakeBackend({
      request_access: () => {
        throw { code: 'open_failed' };
      },
    });
    const onError = vi.fn();
    show('mac', { onError });
    fireEvent.click(screen.getByRole('button', { name: 'Pedir a senha de administrador' }));
    await vi.waitFor(() => expect(onError).toHaveBeenCalledWith({ code: 'open_failed' }));
  });
});
