int main(void) {
  int small[2];
  int big[50];
  int i;
  small[0] = 1;
  big[0] = 100;
  big[49] = 999;
  i = 7;
  return small[0] + big[0] + big[49] + i;
}
