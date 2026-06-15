int main(void) {
  int i = 100;
  long l = 1000000L;
  long r = l + i;
  return (int)(r >> 16);
}
