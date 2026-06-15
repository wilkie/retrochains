enum Color { RED = 1, GREEN, BLUE };

int pick(int v) {
  if (v == RED) return 100;
  if (v == GREEN) return 200;
  return 300;
}
