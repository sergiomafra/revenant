import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import App from './App';
import { HD, SD, SYSTEM } from './test-disks';
import { fakeBackend } from './test-utils';

const settings = () => ({ theme: 'auto', language: 'pt-BR' });

/** `request_access` answers each call with the next item: an object is the access, a string rejects with that code. */
function backend(access: (object | string)[], list = [SD, HD, SYSTEM]) {
  let n = 0;
  return fakeBackend({
    get_settings: settings,
    set_settings: () => null,
    list_disks: () => list,
    request_access: () => {
      const a = access[Math.min(n++, access.length - 1)];
      if (typeof a === 'string') throw { code: a };
      return a;
    },
  });
}

async function chooseSdAndAsk() {
  fireEvent.click(await screen.findByRole('radio', { name: /EOS_DIGITAL/ }));
  fireEvent.click(screen.getByRole('button', { name: 'Continuar' }));
  fireEvent.click(await screen.findByRole('button', { name: 'Pedir a senha de administrador' }));
}

describe('App', () => {
  it('renders the app root once the settings are loaded', async () => {
    fakeBackend({ get_settings: () => ({ theme: 'dark', language: 'es' }), list_disks: () => [] });
    render(<App />);
    expect(await screen.findByTestId('app')).toBeInTheDocument();
    expect(document.documentElement).toHaveAttribute('data-theme', 'dark');
    expect(document.documentElement).toHaveAttribute('lang', 'es');
  });

  it('opens on Choose the disk, with Settings and About in the header', async () => {
    backend([]);
    render(<App />);
    expect(await screen.findByRole('heading', { name: 'O que você quer recuperar?' })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Ajustes' }));
    expect(await screen.findByRole('dialog', { name: 'Ajustes' })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Concluir' }));
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Sobre' }));
    expect(await screen.findByRole('dialog', { name: 'Revenant' })).toBeInTheDocument();
  });

  it('first_load_failure_shows_error_screen', async () => {
    fakeBackend({
      get_settings: settings,
      list_disks: () => {
        throw { code: 'system_tool_failed' };
      },
    });
    render(<App />);
    expect(await screen.findByRole('heading', { name: 'Não deu para ver os discos conectados' })).toBeInTheDocument();
    fireEvent.click(screen.getByText('Detalhes técnicos'));
    expect(screen.getByText('system_tool_failed')).toBeVisible();
  });

  it('permission_asks_and_shows_access_granted', async () => {
    const calls = backend([{ id: 'disk4', size: 64_000_000_000 }]);
    render(<App />);
    await chooseSdAndAsk();
    expect(
      await screen.findByRole('heading', { name: 'O Revenant consegue ler o EOS_DIGITAL (64 GB).' }),
    ).toBeInTheDocument();
    expect(screen.getByText('A busca chega na próxima versão.')).toBeInTheDocument();
    expect(calls.filter((c) => c.cmd === 'request_access')).toEqual([
      { cmd: 'request_access', args: { diskId: 'disk4' } },
    ]);
    fireEvent.click(screen.getByRole('button', { name: 'Voltar' }));
    expect(await screen.findByRole('heading', { name: 'O que você quer recuperar?' })).toBeInTheDocument();
  });

  it('retry_after_denied_asks_again', async () => {
    const calls = backend(['permission_denied', { id: 'disk4', size: 64_000_000_000 }]);
    render(<App />);
    await chooseSdAndAsk();
    expect(
      await screen.findByRole('heading', { name: 'Sem a permissão não dá para ler o cartão' }),
    ).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Tentar de novo' }));
    expect(await screen.findByRole('heading', { name: /O Revenant consegue ler o EOS_DIGITAL/ })).toBeInTheDocument();
    expect(calls.filter((c) => c.cmd === 'request_access')).toHaveLength(2);
  });

  it('disk_gone_returns_to_list_with_notice', async () => {
    backend(['device_not_found']);
    render(<App />);
    await chooseSdAndAsk();
    expect(
      await screen.findByText('O disco não foi encontrado. Confira se ele continua conectado.'),
    ).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'O que você quer recuperar?' })).toBeInTheDocument();
  });

  it('another access error shows the error screen', async () => {
    backend(['open_failed']);
    render(<App />);
    await chooseSdAndAsk();
    expect(await screen.findByRole('heading', { name: 'Não deu para abrir o cartão' })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Voltar ao início' }));
    expect(await screen.findByRole('heading', { name: 'O que você quer recuperar?' })).toBeInTheDocument();
  });
});
