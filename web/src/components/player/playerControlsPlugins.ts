import Plugin, { POSITIONS } from 'xgplayer/es/plugin/plugin.js';

import { invoke } from '@tauri-apps/api/core';

import {
  ICONS,
  loadRoomVolume,
  loadStoredVolume,
  persistRoomVolume,
  persistStoredVolume
} from './constants';

export class VolumeControl extends Plugin {
  static override pluginName = 'volumeControl';
  static override defaultConfig = {
    position: POSITIONS.CONTROLS_LEFT,
    index: 3,
    disable: false,
  };

  private volumeIcon: HTMLElement | null = null;
  private slider: HTMLInputElement | null = null;
  private valueLabel: HTMLElement | null = null;
  private onVolumeChange: ((value: number) => void) | null = null;
  private handleIconClick: ((event: Event) => void) | null = null;
  private previousVolume = 1;
  private roomKey: string | null = null;
  private updateUIFn: ((volume: number) => void) | null = null;
  /** setRoomKey 程序化设置音量/静音会异步触发 volumechange（改 muted 也会
   * 触发一次）；这些事件绝不能回写存储，否则全局默认会被「路过」的直播间
   * 音量带跑。用户真正操作滑块/静音按钮时清除该标志。 */
  private applying = false;
  private appliedValue = 0;
  private saveLogTimer: ReturnType<typeof setTimeout> | null = null;

  /** 切换到某个直播间：应用该房间记忆过的音量（没调过就用全局默认）。 */
  setRoomKey(roomKey: string | null) {
    if (this.roomKey === roomKey) {
      return;
    }
    this.roomKey = roomKey;
    const stored = roomKey ? loadRoomVolume(roomKey) : null;
    if (stored === null) {
      return;
    }
    if (stored > 0) {
      this.previousVolume = stored;
    }
    this.applying = true;
    this.appliedValue = stored;
    this.player.volume = stored;
    // 静音状态必须跟着房间记忆走：上一个房间静音留下的 muted=true 若不清掉，
    // 切到任何房间都没声音（用户只能靠拖滑块自救）。
    this.player.muted = stored === 0;
    this.updateUIFn?.(this.player.volume ?? stored);
    invoke('frontend_log', {
      msg: `[player] room volume applied [${roomKey}] -> ${Math.round(stored * 100)}%`
    }).catch(() => {});
  }

  /** 全局 + 当前直播间一起记住；日志去抖，避免拖动滑块时刷屏。 */
  private persistVolume(volume: number) {
    if (this.applying) {
      return;
    }
    // 全局默认只记「有声」的音量：某个房间静音不应该把全局默认写成 0，
    // 否则没记忆的房间全部开局静音。房间级记忆照常存 0。
    if (volume > 0) {
      persistStoredVolume(volume);
    }
    if (this.roomKey) {
      persistRoomVolume(this.roomKey, volume);
      const key = this.roomKey;
      if (this.saveLogTimer) {
        clearTimeout(this.saveLogTimer);
      }
      this.saveLogTimer = setTimeout(() => {
        invoke('frontend_log', {
          msg: `[player] room volume saved [${key}] -> ${Math.round(volume * 100)}%`
        }).catch(() => {});
      }, 600);
    }
  }

  override render() {
    if (this.config.disable) {
      return '';
    }
    return `<xg-icon class="xgplayer-volume-control" title="音量/静音切换">
      <div class="volume-icon">
        ${ICONS.volume2}
      </div>
      <input class="volume-slider" type="range" min="0" max="100" step="1" value="100">
      <span class="volume-value">100%</span>
    </xg-icon>`;
  }

  override afterCreate() {
    if (this.config.disable) {
      return;
    }
    this.volumeIcon = this.find('.volume-icon') as HTMLElement | null;
    this.slider = this.find('.volume-slider') as HTMLInputElement | null;
    this.valueLabel = this.find('.volume-value') as HTMLElement | null;

    const updateUI = (volume: number) => {
      const clamped = Math.max(0, Math.min(1, volume));
      if (clamped > 0) {
        this.previousVolume = clamped;
      }
      if (this.slider) {
        this.slider.value = String(Math.round(clamped * 100));
        this.updateSliderVisual(this.slider);
      }
      if (this.valueLabel) {
        this.valueLabel.textContent = `${Math.round(clamped * 100)}%`;
      }
      if (this.volumeIcon) {
        this.volumeIcon.setAttribute('data-muted', clamped === 0 ? 'true' : 'false');
      }
    };
    this.updateUIFn = updateUI;

    const storedVolume = loadStoredVolume();
    if (storedVolume !== null) {
      if (storedVolume > 0) {
        this.previousVolume = storedVolume;
      }
      this.player.volume = storedVolume;
      this.player.muted = storedVolume === 0 ? true : this.player.muted;
    } else {
      const initial = this.player.volume ?? 1;
      if (initial > 0) {
        this.previousVolume = initial;
      }
    }

    updateUI(this.player.volume ?? storedVolume ?? 1);

    this.slider?.addEventListener('input', (event) => {
      this.applying = false;
      const value = Number((event.target as HTMLInputElement).value);
      const clampedPercent = Math.max(0, Math.min(100, value));
      const normalized = clampedPercent / 100;
      if (normalized === 0) {
        this.player.muted = true;
      } else {
        this.player.muted = false;
        this.previousVolume = normalized;
      }
      this.player.volume = normalized;
      updateUI(normalized);
      this.persistVolume(normalized);
    });

    this.handleIconClick = (event: Event) => {
      event.preventDefault();
      event.stopPropagation();
      this.applying = false;
      const currentVolume = this.player.volume ?? 0;
      if (currentVolume > 0) {
        this.previousVolume = currentVolume;
        this.player.volume = 0;
        this.player.muted = true;
        updateUI(0);
        this.persistVolume(0);
      } else {
        const restoreVolume = this.previousVolume > 0 ? this.previousVolume : 1;
        this.player.muted = false;
        this.player.volume = restoreVolume;
        updateUI(restoreVolume);
        this.persistVolume(restoreVolume);
      }
    };

    this.volumeIcon?.addEventListener('click', this.handleIconClick);
    if (this.volumeIcon) {
      this.volumeIcon.setAttribute('title', '点击静音 / 取消静音（音量按直播间记忆）');
      this.volumeIcon.style.cursor = 'pointer';
    }

    this.onVolumeChange = () => {
      const current = this.player.volume ?? 1;
      updateUI(current);
      // 程序化应用只会产生「值不变」的 volumechange（改 volume / 改 muted 各
      // 一次）；值变了说明是别的途径（键盘等）在改，恢复持久化。
      if (this.applying && Math.abs(current - this.appliedValue) > 0.001) {
        this.applying = false;
      }
      this.persistVolume(current);
    };
    this.player.on('volumechange', this.onVolumeChange);
  }

  override destroy() {
    if (this.handleIconClick && this.volumeIcon) {
      this.volumeIcon.removeEventListener('click', this.handleIconClick);
      this.handleIconClick = null;
    }
    if (this.onVolumeChange) {
      this.player.off('volumechange', this.onVolumeChange);
      this.onVolumeChange = null;
    }
    if (this.saveLogTimer) {
      clearTimeout(this.saveLogTimer);
      this.saveLogTimer = null;
    }
    this.updateUIFn = null;
    this.roomKey = null;
    this.volumeIcon = null;
    this.slider = null;
    this.valueLabel = null;
  }

  private updateSliderVisual(el: HTMLInputElement | null) {
    if (!el) {
      return;
    }
    const value = Number(el.value);
    const percent = Math.max(0, Math.min(100, value));
    el.style.background = `linear-gradient(90deg, var(--player-accent) ${percent}%, rgba(255, 255, 255, 0.15) ${percent}%)`;
  }
}

export class RefreshControl extends Plugin {
  static override pluginName = 'refreshControl';
  static override defaultConfig = {
    position: POSITIONS.CONTROLS_LEFT,
    index: 2,
    disable: false,
    onClick: null as (() => void) | null,
  };

  private handleClick: ((event: Event) => void) | null = null;
  private isLoading = false;

  override afterCreate() {
    if (this.config.disable) {
      return;
    }
    this.handleClick = (event: Event) => {
      event.preventDefault();
      event.stopPropagation();
      if (this.isLoading) {
        return;
      }
      if (typeof this.config.onClick === 'function') {
        this.config.onClick();
      }
    };
    this.bind(['click', 'touchend'], this.handleClick);
  }

  override destroy() {
    if (this.handleClick) {
      this.unbind(['click', 'touchend'], this.handleClick);
      this.handleClick = null;
    }
    this.setLoading(false);
  }

  override render() {
    if (this.config.disable) {
      return '';
    }
    return `<xg-icon class="xgplayer-refresh-control" title="刷新">
      ${ICONS.rotateCcw}
    </xg-icon>`;
  }

  setLoading(isLoading: boolean) {
    this.isLoading = isLoading;
    const root = this.root as HTMLElement | null;
    if (!root) {
      return;
    }
    root.classList.toggle('is-loading', isLoading);
    if (isLoading) {
      root.setAttribute('aria-disabled', 'true');
    } else {
      root.removeAttribute('aria-disabled');
    }
  }
}

export class QualityControl extends Plugin {
  static override pluginName = 'qualityControl';
  static override defaultConfig = {
    position: POSITIONS.CONTROLS_RIGHT,
    index: 5,
    disable: false,
    options: [] as string[],
    getCurrent: (() => '') as () => string,
    onSelect: (async (_value: string) => {}) as (value: string) => Promise<void> | void,
  };

  private dropdown: HTMLElement | null = null;
  private handleToggle: ((event: Event) => void) | null = null;
  private handleDocumentClick: ((event: MouseEvent) => void) | null = null;
  private handleHoverEnter: ((event: Event) => void) | null = null;
  private handleHoverLeave: ((event: Event) => void) | null = null;
  private hoverCloseTimer: ReturnType<typeof setTimeout> | null = null;
  private isSwitching = false;

  override afterCreate() {
    if (this.config.disable) {
      return;
    }

    this.createDropdown();
    this.updateLabel(this.getCurrent());

    this.handleToggle = (event: Event) => {
      event.preventDefault();
      event.stopPropagation();
      if (this.isSwitching) {
        return;
      }
      this.toggleDropdown();
    };
    this.bind(['click', 'touchend'], this.handleToggle);

    if (typeof document !== 'undefined') {
      this.handleDocumentClick = (event: MouseEvent) => {
        if (this.root && !this.root.contains(event.target as Node)) {
          this.hideDropdown();
        }
      };
      document.addEventListener('click', this.handleDocumentClick);
    }

    this.handleHoverEnter = () => {
      if (this.hoverCloseTimer) {
        clearTimeout(this.hoverCloseTimer);
        this.hoverCloseTimer = null;
      }
      if (!this.isSwitching) {
        this.openDropdown();
      }
    };
    this.handleHoverLeave = () => {
      if (this.hoverCloseTimer) {
        clearTimeout(this.hoverCloseTimer);
      }
      this.hoverCloseTimer = setTimeout(() => {
        this.hoverCloseTimer = null;
        this.hideDropdown();
      }, 220);
    };
    this.bind('mouseenter', this.handleHoverEnter);
    this.bind('mouseleave', this.handleHoverLeave);
  }

  override destroy() {
    if (this.handleToggle) {
      this.unbind(['click', 'touchend'], this.handleToggle);
      this.handleToggle = null;
    }
    if (this.handleDocumentClick) {
      document.removeEventListener('click', this.handleDocumentClick);
      this.handleDocumentClick = null;
    }
    if (this.handleHoverEnter) {
      this.unbind('mouseenter', this.handleHoverEnter);
      this.handleHoverEnter = null;
    }
    if (this.handleHoverLeave) {
      this.unbind('mouseleave', this.handleHoverLeave);
      this.handleHoverLeave = null;
    }
    if (this.hoverCloseTimer) {
      clearTimeout(this.hoverCloseTimer);
      this.hoverCloseTimer = null;
    }
    if (this.dropdown) {
      this.dropdown.remove();
      this.dropdown = null;
    }
    this.setSwitching(false);
  }

  override render() {
    if (this.config.disable) {
      return '';
    }
    const current = this.getCurrent();
    return `<xg-icon class="xgplayer-quality-control" title="">
      <span class="quality-label">${current}</span>
      <svg class="quality-caret" width="10" height="10" viewBox="0 0 10 10" fill="none">
        <path d="M2.5 3.5L5 6l2.5-2.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/>
      </svg>
    </xg-icon>`;
  }

  updateLabel(label: string) {
    const textEl = this.find('.quality-label') as HTMLElement | null;
    if (textEl) {
      textEl.textContent = label;
    }
    this.updateActiveState(label);
  }

  setOptions(options: string[]) {
    this.config.options = options;
    this.populateDropdown();
  }

  private getCurrent() {
    return typeof this.config.getCurrent === 'function' ? this.config.getCurrent() : '';
  }

  private createDropdown() {
    this.dropdown = document.createElement('div');
    this.dropdown.className = 'xgplayer-quality-dropdown';
    this.root?.appendChild(this.dropdown);
    this.populateDropdown();
  }

  private populateDropdown() {
    if (!this.dropdown) {
      return;
    }
    this.dropdown.innerHTML = '';
    const options: string[] = Array.isArray(this.config.options) ? this.config.options : [];
    options.forEach((option) => {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'xgplayer-quality-item';
      btn.innerHTML = `
        <span class="quality-name">${option}</span>
        <svg class="quality-check" width="12" height="12" viewBox="0 0 12 12" fill="none">
          <path d="M3 6.5l2 2 4-4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
        </svg>
      `;
      btn.addEventListener('click', (event) => {
        event.stopPropagation();
        event.preventDefault();
        if (this.isSwitching) {
          return;
        }
        let actionResult: Promise<void> | void;
        try {
          const callback = this.config.onSelect;
          actionResult = typeof callback === 'function' ? callback(option) : undefined;
        } catch (error) {
          console.error('[QualityControl] onSelect error:', error);
          actionResult = undefined;
        }
        Promise.resolve(actionResult).finally(() => {
          this.hideDropdown();
          this.updateLabel(this.getCurrent());
        });
      });
      this.dropdown!.appendChild(btn);
    });
    this.updateActiveState(this.getCurrent());
    this.applySwitchingState();
  }

  private toggleDropdown() {
    if (this.isSwitching) {
      return;
    }
    if (this.dropdown?.classList.contains('show')) {
      this.hideDropdown();
    } else {
      this.openDropdown();
    }
  }

  private openDropdown() {
    if (this.isSwitching || !this.dropdown) {
      return;
    }
    if (this.hoverCloseTimer) {
      clearTimeout(this.hoverCloseTimer);
      this.hoverCloseTimer = null;
    }
    this.dropdown.classList.add('show');
    this.root?.classList.add('menu-open');
    this.updateActiveState(this.getCurrent());
  }

  private hideDropdown() {
    if (this.hoverCloseTimer) {
      clearTimeout(this.hoverCloseTimer);
      this.hoverCloseTimer = null;
    }
    if (this.dropdown) {
      this.dropdown.classList.remove('show');
    }
    this.root?.classList.remove('menu-open');
  }

  private updateActiveState(current: string) {
    if (!this.dropdown) {
      return;
    }
    const items = this.dropdown.querySelectorAll<HTMLButtonElement>('.xgplayer-quality-item');
    items.forEach((item) => {
      const label = item.querySelector('.quality-name')?.textContent?.trim();
      item.classList.toggle('active', label === current);
    });
  }

  setSwitching(isSwitching: boolean) {
    this.isSwitching = isSwitching;
    this.applySwitchingState();
    if (isSwitching) {
      this.hideDropdown();
    }
  }

  private applySwitchingState() {
    const root = this.root as HTMLElement | null;
    if (root) {
      root.classList.toggle('is-switching', this.isSwitching);
    }
    if (this.dropdown) {
      this.dropdown.classList.toggle('disabled', this.isSwitching);
      const buttons = this.dropdown.querySelectorAll<HTMLButtonElement>('.xgplayer-quality-item');
      buttons.forEach((button) => {
        button.disabled = this.isSwitching;
      });
    }
  }
}

export interface LineOption {
  key: string;
  label: string;
}

export class LineControl extends Plugin {
  static override pluginName = 'lineControl';
  static override defaultConfig = {
    position: POSITIONS.CONTROLS_RIGHT,
    index: 5.2,
    disable: false,
    options: [] as LineOption[],
    getCurrentKey: (() => '') as () => string,
    getCurrentLabel: (() => '线路') as () => string,
    onSelect: (async (_value: string) => {}) as (value: string) => Promise<void> | void,
  };

  private dropdown: HTMLElement | null = null;
  private handleToggle: ((event: Event) => void) | null = null;
  private handleDocumentClick: ((event: MouseEvent) => void) | null = null;
  private handleHoverEnter: ((event: Event) => void) | null = null;
  private handleHoverLeave: ((event: Event) => void) | null = null;
  private hoverCloseTimer: ReturnType<typeof setTimeout> | null = null;
  private isSwitching = false;

  override afterCreate() {
    if (this.config.disable) {
      this.updateVisibility();
      return;
    }

    this.createDropdown();
    this.updateLabel(this.getCurrentLabel());
    this.updateVisibility();

    this.handleToggle = (event: Event) => {
      event.preventDefault();
      event.stopPropagation();
      if (this.isSwitching) {
        return;
      }
      this.toggleDropdown();
    };
    this.bind(['click', 'touchend'], this.handleToggle);

    if (typeof document !== 'undefined') {
      this.handleDocumentClick = (event: MouseEvent) => {
        if (this.root && !this.root.contains(event.target as Node)) {
          this.hideDropdown();
        }
      };
      document.addEventListener('click', this.handleDocumentClick);
    }

    this.handleHoverEnter = () => {
      if (this.hoverCloseTimer) {
        clearTimeout(this.hoverCloseTimer);
        this.hoverCloseTimer = null;
      }
      if (!this.isSwitching) {
        this.openDropdown();
      }
    };
    this.handleHoverLeave = () => {
      if (this.hoverCloseTimer) {
        clearTimeout(this.hoverCloseTimer);
      }
      this.hoverCloseTimer = setTimeout(() => {
        this.hoverCloseTimer = null;
        this.hideDropdown();
      }, 220);
    };
    this.bind('mouseenter', this.handleHoverEnter);
    this.bind('mouseleave', this.handleHoverLeave);
  }

  override destroy() {
    if (this.handleToggle) {
      this.unbind(['click', 'touchend'], this.handleToggle);
      this.handleToggle = null;
    }
    if (this.handleDocumentClick) {
      document.removeEventListener('click', this.handleDocumentClick);
      this.handleDocumentClick = null;
    }
    if (this.handleHoverEnter) {
      this.unbind('mouseenter', this.handleHoverEnter);
      this.handleHoverEnter = null;
    }
    if (this.handleHoverLeave) {
      this.unbind('mouseleave', this.handleHoverLeave);
      this.handleHoverLeave = null;
    }
    if (this.hoverCloseTimer) {
      clearTimeout(this.hoverCloseTimer);
      this.hoverCloseTimer = null;
    }
    if (this.dropdown) {
      this.dropdown.remove();
      this.dropdown = null;
    }
    this.setSwitching(false);
  }

  override render() {
    if (this.config.disable) {
      return '';
    }
    const current = this.getCurrentLabel();
    return `<xg-icon class="xgplayer-line-control" title="">
      <span class="line-label">${current || '线路'}</span>
      <svg class="line-caret" width="10" height="10" viewBox="0 0 10 10" fill="none">
        <path d="M2.5 3.5L5 6l2.5-2.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/>
      </svg>
    </xg-icon>`;
  }

  updateLabel(label: string) {
    const textEl = this.find('.line-label') as HTMLElement | null;
    if (textEl) {
      textEl.textContent = label || '线路';
    }
    this.updateActiveState(this.getCurrentKey());
  }

  setOptions(options: LineOption[]) {
    this.config.options = Array.isArray(options) ? [...options] : [];
    this.populateDropdown();
    this.updateVisibility();
  }

  setSwitching(isSwitching: boolean) {
    this.isSwitching = isSwitching;
    this.applySwitchingState();
    if (isSwitching) {
      this.hideDropdown();
    }
  }

  private getCurrentKey() {
    return typeof this.config.getCurrentKey === 'function'
      ? this.config.getCurrentKey()
      : '';
  }

  private getCurrentLabel() {
    return typeof this.config.getCurrentLabel === 'function'
      ? this.config.getCurrentLabel()
      : '线路';
  }

  private createDropdown() {
    this.dropdown = document.createElement('div');
    this.dropdown.className = 'xgplayer-line-dropdown';
    this.root?.appendChild(this.dropdown);
    this.populateDropdown();
  }

  private populateDropdown() {
    if (!this.dropdown) {
      return;
    }
    this.dropdown.innerHTML = '';
    const options: LineOption[] = Array.isArray(this.config.options) ? this.config.options : [];
    options.forEach((option) => {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'xgplayer-quality-item';
      btn.dataset.lineKey = option.key;
      btn.innerHTML = `
        <span class="quality-name">${option.label}</span>
        <svg class="quality-check" width="12" height="12" viewBox="0 0 12 12" fill="none">
          <path d="M3 6.5l2 2 4-4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
        </svg>
      `;
      btn.addEventListener('click', (event) => {
        event.stopPropagation();
        event.preventDefault();
        if (this.isSwitching) {
          return;
        }
        let actionResult: Promise<void> | void;
        try {
          const callback = this.config.onSelect;
          actionResult = typeof callback === 'function' ? callback(option.key) : undefined;
        } catch (error) {
          console.error('[LineControl] onSelect error:', error);
          actionResult = undefined;
        }
        Promise.resolve(actionResult).finally(() => {
          this.hideDropdown();
          this.updateLabel(this.getCurrentLabel());
        });
      });
      this.dropdown!.appendChild(btn);
    });
    this.updateActiveState(this.getCurrentKey());
    this.applySwitchingState();
  }

  private toggleDropdown() {
    if (this.isSwitching) {
      return;
    }
    if (this.dropdown?.classList.contains('show')) {
      this.hideDropdown();
    } else {
      this.openDropdown();
    }
  }

  private openDropdown() {
    if (this.isSwitching || !this.dropdown) {
      return;
    }
    if (this.hoverCloseTimer) {
      clearTimeout(this.hoverCloseTimer);
      this.hoverCloseTimer = null;
    }
    this.dropdown.classList.add('show');
    this.root?.classList.add('menu-open');
    this.updateActiveState(this.getCurrentKey());
  }

  private hideDropdown() {
    if (this.hoverCloseTimer) {
      clearTimeout(this.hoverCloseTimer);
      this.hoverCloseTimer = null;
    }
    if (this.dropdown) {
      this.dropdown.classList.remove('show');
    }
    this.root?.classList.remove('menu-open');
  }

  private updateActiveState(currentKey: string) {
    if (!this.dropdown) {
      return;
    }
    const items = this.dropdown.querySelectorAll<HTMLButtonElement>('.xgplayer-quality-item');
    items.forEach((item) => {
      const key = item.dataset.lineKey ?? '';
      item.classList.toggle('active', key === currentKey);
    });
  }

  private applySwitchingState() {
    const root = this.root as HTMLElement | null;
    if (root) {
      root.classList.toggle('is-switching', this.isSwitching);
    }
    if (this.dropdown) {
      this.dropdown.classList.toggle('disabled', this.isSwitching);
      const buttons = this.dropdown.querySelectorAll<HTMLButtonElement>('.xgplayer-quality-item');
      buttons.forEach((button) => {
        button.disabled = this.isSwitching;
      });
    }
  }

  private updateVisibility() {
    const root = this.root as HTMLElement | null;
    if (!root) {
      return;
    }
    const options: LineOption[] = Array.isArray(this.config.options) ? this.config.options : [];
    root.style.display = options.length === 0 ? 'none' : '';
  }
}

export class CastControl extends Plugin {
  static override pluginName = 'castControl';
  static override defaultConfig = {
    position: POSITIONS.CONTROLS_RIGHT,
    index: 6,
    disable: false,
    onClick: null as (() => void) | null,
  };

  private handleClick: ((event: Event) => void) | null = null;

  override afterCreate() {
    if (this.config.disable) {
      return;
    }
    this.handleClick = (event: Event) => {
      event.preventDefault();
      event.stopPropagation();
      if (typeof this.config.onClick === 'function') {
        this.config.onClick();
      }
    };
    this.bind(['click', 'touchend'], this.handleClick);
  }

  override destroy() {
    if (this.handleClick) {
      this.unbind(['click', 'touchend'], this.handleClick);
      this.handleClick = null;
    }
  }

  override render() {
    if (this.config.disable) {
      return '';
    }
    return `<xg-icon class="xgplayer-cast-control" title="投屏">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" width="18" height="18">
        <rect x="2" y="7" width="20" height="15" rx="2" ry="2"/>
        <polyline points="17 2 12 7 7 2"/>
      </svg>
    </xg-icon>`;
  }
}
