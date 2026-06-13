int sum_row(int (*row)[3]) {
  return (*row)[0] + (*row)[1] + (*row)[2];
}
int main(void) {
  int m[2][3];
  m[0][0] = 1; m[0][1] = 2; m[0][2] = 3;
  m[1][0] = 4; m[1][1] = 5; m[1][2] = 6;
  return sum_row(&m[1]);
}
