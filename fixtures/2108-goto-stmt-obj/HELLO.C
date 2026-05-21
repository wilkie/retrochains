int main(void) {
  int x = 0;
  start:
  x++;
  if (x < 5) goto start;
  return x;
}
