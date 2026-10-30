import * as migration_20261030_055158_init from './20261030_055158_init';

export const migrations = [
  {
    up: migration_20261030_055158_init.up,
    down: migration_20261030_055158_init.down,
    name: '20261030_055158_init'
  },
];
