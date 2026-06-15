int main(void) {
  int y;
  int z;
  int r;
  y = 0;
  z = 0;
  r = y ? y++ : z--;
  return r + y + z;
}
