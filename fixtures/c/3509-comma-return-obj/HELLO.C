int g;

int sneaky(int x) {
  return (g = x, g + 1);
}
