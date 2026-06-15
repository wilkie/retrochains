enum step { S0, S1, S2 = 10, S3, S4 = 20, S5 };
int main(void) {
  int a = S0 + S1;
  int b = S2 * 2 - S3;
  int c = S5 - S4;
  return a + b + c;
}
