int main(void) {
  int big[80];
  big[0] = 1;
  big[79] = 99;
  return big[0] + big[79];
}
