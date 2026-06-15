int main(void) {
  static char *names[3] = {"alpha", "beta", "gamma"};
  return names[0][0] + names[1][0] + names[2][0];
}
